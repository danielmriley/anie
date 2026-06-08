//! Content-addressed working-tree checkpoint store for rewind.
//!
//! A [`WorkspaceCheckpointStore`] is a sidecar to a session's JSONL
//! file. It snapshots the on-disk content of a bounded set of tracked
//! paths at each capture point and can restore the working tree to any
//! captured point — the filesystem half of session "rewind". The
//! conversation half is the existing append-only
//! [`crate::SessionManager::fork`]; this module is deliberately
//! independent of the message log and does **not** touch the session
//! schema (binary file content does not belong in the message JSONL).
//!
//! ## Layout
//!
//! ```text
//! <sessions_dir>/<session_id>.checkpoints/
//!   blobs/<sha256-hex>   # one file per unique content, deduplicated
//!   manifest.json        # ordered list of capture entries
//! ```
//!
//! The manifest maps each `entry_id` to the [`FileState`] of every
//! tracked path at that capture: `Blob(hash)` when the file existed,
//! `Absent` when it did not. Content addressing means an unchanged file
//! costs one hash and zero extra bytes across captures.
//!
//! ## Tracked set
//!
//! Callers pass the path set explicitly; in anie this is the union of
//! [`crate::CompactionDetails::modified_files`] across the session
//! branch, so the rewind store and the compaction summary agree on
//! "what the agent touched." Bash-driven mutations (`mv`, `>`, `rm`)
//! are out of scope by design — they are not in the tracked
//! `write`/`edit` set.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Errors returned by the checkpoint store.
#[derive(Debug, Error)]
pub enum CheckpointError {
    /// A tracked file changed on disk since its last capture, so a
    /// restore would silently destroy uncaptured work. Refused.
    #[error(
        "working tree has drifted at {path}; refusing to overwrite \
         changes made since the last checkpoint"
    )]
    WorkingTreeDrifted {
        /// The path whose on-disk content no longer matches the most
        /// recent captured state.
        path: String,
    },
    /// No capture was recorded for the requested entry id.
    #[error("no checkpoint recorded for entry {0}")]
    UnknownCheckpoint(String),
    /// A blob referenced by the manifest is missing from `blobs/`.
    #[error("checkpoint blob {0} is missing from the store")]
    MissingBlob(String),
    /// Underlying filesystem error.
    #[error("checkpoint store I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// The captured state of a single tracked path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "hash")]
pub enum FileState {
    /// The file existed; its content hashes to this sha256 hex digest.
    Blob(String),
    /// The file did not exist at this capture point.
    Absent,
}

/// One capture point: the state of every tracked path at an entry id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// The session entry id this capture is keyed to (a user-turn id
    /// or a named-checkpoint anchor).
    pub entry_id: String,
    /// Optional user-supplied label (set by `/checkpoint <name>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// State of each tracked path, keyed by the path string the caller
    /// passed (relative to the workspace root, or absolute).
    pub files: BTreeMap<String, FileState>,
    /// Internal drift baseline recorded right after a restore — not a
    /// user-selectable rewind anchor. It keeps `latest_state` in sync
    /// with the tree a restore just wrote, so an immediate second rewind
    /// isn't mistaken for user drift. Excluded from the `/rewind` listing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub baseline_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Manifest {
    /// Capture points in insertion (chronological) order.
    entries: Vec<ManifestEntry>,
}

/// What a [`WorkspaceCheckpointStore::restore`] did to the working
/// tree, for display and assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// Paths rewritten to a prior blob.
    pub written: Vec<String>,
    /// Paths deleted because they were absent at the target.
    pub deleted: Vec<String>,
}

/// A content-addressed shadow store for working-tree rewind.
pub struct WorkspaceCheckpointStore {
    /// The `.checkpoints/` sidecar directory.
    root: PathBuf,
    /// Working-tree root that tracked paths resolve against.
    workspace_root: PathBuf,
    manifest: Manifest,
}

impl WorkspaceCheckpointStore {
    /// Open (or create) a checkpoint store at `checkpoint_dir`, with
    /// tracked paths resolved against `workspace_root`. An existing
    /// `manifest.json` is loaded; a missing one starts empty.
    pub fn open(
        checkpoint_dir: impl AsRef<Path>,
        workspace_root: impl AsRef<Path>,
    ) -> Result<Self, CheckpointError> {
        let root = checkpoint_dir.as_ref().to_path_buf();
        fs::create_dir_all(root.join("blobs"))?;
        let manifest = match fs::read(root.join("manifest.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| CheckpointError::Io(std::io::Error::new(ErrorKind::InvalidData, e)))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Manifest::default(),
            Err(e) => return Err(e.into()),
        };
        let mut store = Self {
            root,
            workspace_root: workspace_root.as_ref().to_path_buf(),
            manifest,
        };
        // Compact any accumulated drift baselines on open (cheap in-memory
        // pass; a missing-fix or long-lived manifest can carry stale ones).
        // Persist only when something was actually removed.
        if store.gc_shadowed_baselines() {
            store.persist_manifest()?;
        }
        Ok(store)
    }

    /// The capture points recorded so far, in chronological order.
    #[must_use]
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.manifest.entries
    }

    /// Snapshot the current on-disk content of every path in
    /// `tracked_paths`, keyed by `entry_id`. Unchanged content is
    /// deduplicated by hash. Re-capturing an existing `entry_id`
    /// replaces its record.
    pub fn capture(
        &mut self,
        entry_id: &str,
        tracked_paths: &[String],
        label: Option<String>,
    ) -> Result<(), CheckpointError> {
        let mut files = BTreeMap::new();
        for path in tracked_paths {
            let state = match fs::read(self.resolve(path)) {
                Ok(bytes) => {
                    let hash = hex_digest(&bytes);
                    self.write_blob(&hash, &bytes)?;
                    FileState::Blob(hash)
                }
                Err(e) if e.kind() == ErrorKind::NotFound => FileState::Absent,
                Err(e) => return Err(e.into()),
            };
            files.insert(path.clone(), state);
        }
        let entry = ManifestEntry {
            entry_id: entry_id.to_string(),
            label,
            files,
            baseline_only: false,
        };
        match self
            .manifest
            .entries
            .iter_mut()
            .find(|e| e.entry_id == entry_id && !e.baseline_only)
        {
            Some(existing) => *existing = entry,
            None => self.manifest.entries.push(entry),
        }
        self.persist_manifest()
    }

    /// After a successful [`restore`](Self::restore) to `entry_id`, record
    /// the now-current (restored) tree state as a fresh, newest drift
    /// baseline. Without this, `latest_state` keeps returning the
    /// pre-rewind capture, so a second consecutive `/rewind` (no prompt
    /// in between) would be falsely refused as `WorkingTreeDrifted`. The
    /// baseline is internal: it is excluded from the rewind-anchor
    /// listing. No-op when `entry_id` has no capture.
    pub fn record_restore_baseline(&mut self, entry_id: &str) -> Result<(), CheckpointError> {
        let Some(files) = self
            .manifest
            .entries
            .iter()
            .find(|e| e.entry_id == entry_id && !e.baseline_only)
            .map(|e| e.files.clone())
        else {
            return Ok(());
        };
        // A synthetic, unique id. Real session entry ids are 8-char hex
        // (`0-9a-f`), which can never contain the `#restored` infix, so a
        // capture/restore lookup never matches a baseline; the `len()`
        // suffix keeps repeated rewinds distinct.
        let id = format!("{entry_id}#restored{}", self.manifest.entries.len());
        self.manifest.entries.push(ManifestEntry {
            entry_id: id,
            label: None,
            files,
            baseline_only: true,
        });
        // The new baseline shadows older ones (per path); drop any that are
        // now fully superseded so the manifest doesn't grow per-rewind.
        self.gc_shadowed_baselines();
        self.persist_manifest()
    }

    /// Remove `baseline_only` entries that are fully shadowed — i.e. every
    /// path they record also appears in a strictly-later entry, so they can
    /// never be returned by [`latest_state`](Self::latest_state) and
    /// contribute nothing. Returns whether any entry was removed.
    ///
    /// Safe and behavior-preserving: each path's newest-mentioning entry is
    /// never fully shadowed (it has no later mention), so it is always kept,
    /// and `latest_state` for every path is unchanged. Only baselines are
    /// removed — real captures are user-facing rewind anchors / restore
    /// targets and stay. This bounds baseline growth by the number of live
    /// tracked paths, not by the number of `/rewind`s.
    fn gc_shadowed_baselines(&mut self) -> bool {
        let entries = &self.manifest.entries;
        let mut remove = vec![false; entries.len()];
        for i in 0..entries.len() {
            if !entries[i].baseline_only {
                continue;
            }
            let fully_shadowed = entries[i].files.keys().all(|path| {
                entries[i + 1..]
                    .iter()
                    .any(|later| later.files.contains_key(path))
            });
            remove[i] = fully_shadowed;
        }
        if !remove.iter().any(|&r| r) {
            return false;
        }
        let mut index = 0;
        self.manifest.entries.retain(|_| {
            let keep = !remove[index];
            index += 1;
            keep
        });
        true
    }

    /// Restore the working tree to the capture keyed by `entry_id`:
    /// rewrite each tracked path to its recorded blob, or delete it if
    /// it was absent. Refuses with [`CheckpointError::WorkingTreeDrifted`]
    /// if any target path changed on disk since its most recent capture,
    /// and with [`CheckpointError::MissingBlob`] if a referenced blob is
    /// absent from the store (no silent clobber). Both checks run for
    /// every path *before* any write, so a refusal leaves the tree
    /// untouched.
    pub fn restore(&self, entry_id: &str) -> Result<RestorePlan, CheckpointError> {
        let entry = self
            .manifest
            .entries
            .iter()
            .find(|e| e.entry_id == entry_id)
            .ok_or_else(|| CheckpointError::UnknownCheckpoint(entry_id.to_string()))?;

        for path in entry.files.keys() {
            if let Some(latest) = self.latest_state(path) {
                if self.current_state(path)? != latest {
                    return Err(CheckpointError::WorkingTreeDrifted { path: path.clone() });
                }
            }
        }

        // Pre-load every referenced blob before touching the tree, so a
        // missing blob (corruption / partial deletion of `blobs/`) refuses
        // as a true no-op rather than leaving a half-restored tree.
        let mut blobs: std::collections::HashMap<&str, Vec<u8>> = std::collections::HashMap::new();
        for state in entry.files.values() {
            if let FileState::Blob(hash) = state
                && !blobs.contains_key(hash.as_str())
            {
                blobs.insert(hash.as_str(), self.read_blob(hash)?);
            }
        }

        let mut plan = RestorePlan::default();
        for (path, state) in &entry.files {
            let resolved = self.resolve(path);
            match state {
                FileState::Blob(hash) => {
                    let bytes = &blobs[hash.as_str()];
                    if let Some(parent) = resolved.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&resolved, bytes)?;
                    plan.written.push(path.clone());
                }
                FileState::Absent => {
                    match fs::remove_file(&resolved) {
                        Ok(()) => {}
                        Err(e) if e.kind() == ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    plan.deleted.push(path.clone());
                }
            }
        }
        Ok(plan)
    }

    /// The most recent captured state for `path`, scanning captures
    /// newest-first. `None` when the path was never tracked.
    fn latest_state(&self, path: &str) -> Option<FileState> {
        self.manifest
            .entries
            .iter()
            .rev()
            .find_map(|entry| entry.files.get(path).cloned())
    }

    /// The current on-disk state of `path` as a [`FileState`].
    fn current_state(&self, path: &str) -> Result<FileState, CheckpointError> {
        match fs::read(self.resolve(path)) {
            Ok(bytes) => Ok(FileState::Blob(hex_digest(&bytes))),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(FileState::Absent),
            Err(e) => Err(e.into()),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        // An absolute `path` replaces the root; a relative one joins it.
        self.workspace_root.join(path)
    }

    fn blob_path(&self, hash: &str) -> PathBuf {
        self.root.join("blobs").join(hash)
    }

    fn write_blob(&self, hash: &str, bytes: &[u8]) -> Result<(), CheckpointError> {
        let path = self.blob_path(hash);
        if path.exists() {
            return Ok(());
        }
        // Write through a temp sibling then rename so a concurrent
        // reader never sees a half-written, mis-addressed blob.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn read_blob(&self, hash: &str) -> Result<Vec<u8>, CheckpointError> {
        match fs::read(self.blob_path(hash)) {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                Err(CheckpointError::MissingBlob(hash.to_string()))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn persist_manifest(&self) -> Result<(), CheckpointError> {
        let bytes = serde_json::to_vec_pretty(&self.manifest)
            .map_err(|e| CheckpointError::Io(std::io::Error::new(ErrorKind::InvalidData, e)))?;
        let path = self.root.join("manifest.json");
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Lowercase hex of the sha256 digest of `bytes`.
fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> WorkspaceCheckpointStore {
        WorkspaceCheckpointStore::open(dir.join(".checkpoints"), dir).unwrap()
    }

    fn blob_count(dir: &Path) -> usize {
        fs::read_dir(dir.join(".checkpoints").join("blobs"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_none_or(|ext| ext != "tmp"))
            .count()
    }

    #[test]
    fn checkpoint_capture_dedupes_identical_blobs_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"same").unwrap();
        fs::write(dir.path().join("b.txt"), b"same").unwrap();
        let mut s = store(dir.path());
        s.capture("turn1", &["a.txt".to_string(), "b.txt".to_string()], None)
            .unwrap();
        // Two paths, identical content -> a single content-addressed blob.
        assert_eq!(blob_count(dir.path()), 1);
    }

    #[test]
    fn checkpoint_restore_rewrites_modified_file_to_prior_blob() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.rs");
        let tracked = vec!["code.rs".to_string()];

        fs::write(&file, b"v1").unwrap();
        let mut s = store(dir.path());
        s.capture("turn1", &tracked, None).unwrap();

        fs::write(&file, b"v2").unwrap();
        s.capture("turn2", &tracked, None).unwrap();

        let plan = s.restore("turn1").unwrap();
        assert_eq!(plan.written, vec!["code.rs".to_string()]);
        assert_eq!(fs::read(&file).unwrap(), b"v1");
    }

    #[test]
    fn checkpoint_restore_deletes_file_absent_at_target() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.rs");
        let tracked = vec!["new.rs".to_string()];

        // turn1: file does not yet exist.
        let mut s = store(dir.path());
        s.capture("turn1", &tracked, None).unwrap();

        // turn2: agent created it.
        fs::write(&file, b"created").unwrap();
        s.capture("turn2", &tracked, None).unwrap();

        let plan = s.restore("turn1").unwrap();
        assert_eq!(plan.deleted, vec!["new.rs".to_string()]);
        assert!(!file.exists());
    }

    #[test]
    fn consecutive_restores_are_not_falsely_refused_as_drift() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.rs");
        let tracked = vec!["code.rs".to_string()];

        fs::write(&file, b"v1").unwrap();
        let mut s = store(dir.path());
        s.capture("t1", &tracked, None).unwrap();
        fs::write(&file, b"v2").unwrap();
        s.capture("t2", &tracked, None).unwrap();

        // First rewind to t1 + record the restored state as the baseline.
        s.restore("t1").unwrap();
        s.record_restore_baseline("t1").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"v1");

        // A second rewind with no capture in between must NOT be refused:
        // the tree (v1) matches the recorded baseline, not the stale t2.
        s.restore("t1")
            .expect("second consecutive rewind must succeed");
        assert_eq!(fs::read(&file).unwrap(), b"v1");

        // A genuine user edit between rewinds is still caught as drift.
        fs::write(&file, b"user-edit").unwrap();
        let err = s.restore("t1").unwrap_err();
        assert!(
            matches!(err, CheckpointError::WorkingTreeDrifted { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn repeated_rewinds_do_not_grow_the_baseline_count() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.rs");
        let tracked = vec!["code.rs".to_string()];

        fs::write(&file, b"v1").unwrap();
        let mut s = store(dir.path());
        s.capture("t1", &tracked, None).unwrap();
        fs::write(&file, b"v2").unwrap();
        s.capture("t2", &tracked, None).unwrap();

        // Many consecutive rewinds: each appends a baseline, but the new one
        // shadows the prior (same path), which GC removes — so the baseline
        // count stays bounded instead of growing per rewind.
        for _ in 0..8 {
            s.restore("t1").unwrap();
            s.record_restore_baseline("t1").unwrap();
        }
        let baselines = s.entries().iter().filter(|e| e.baseline_only).count();
        assert!(
            baselines <= 1,
            "baselines must not grow per rewind: {baselines}"
        );
        // Real captures (the user-facing anchors) are untouched.
        assert_eq!(s.entries().iter().filter(|e| !e.baseline_only).count(), 2);

        // Drift detection still works after GC: tree is v1, a user edit is
        // still refused.
        assert_eq!(fs::read(&file).unwrap(), b"v1");
        fs::write(&file, b"edited").unwrap();
        assert!(matches!(
            s.restore("t1").unwrap_err(),
            CheckpointError::WorkingTreeDrifted { .. }
        ));
    }

    #[test]
    fn open_compacts_legacy_shadowed_baselines() {
        let dir = tempfile::tempdir().unwrap();
        let cp = dir.path().join(".checkpoints");
        fs::create_dir_all(cp.join("blobs")).unwrap();
        // A manifest accumulated by a pre-GC binary: two shadowed baselines.
        let manifest = serde_json::json!({
            "entries": [
                {"entry_id":"t1","files":{"a.rs":{"kind":"blob","hash":"h1"}}},
                {"entry_id":"t1#restored1","files":{"a.rs":{"kind":"blob","hash":"h1"}},"baseline_only":true},
                {"entry_id":"t1#restored2","files":{"a.rs":{"kind":"blob","hash":"h1"}},"baseline_only":true}
            ]
        });
        fs::write(
            cp.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let s = WorkspaceCheckpointStore::open(&cp, dir.path()).unwrap();
        // The older fully-shadowed baseline is dropped; the newest is kept.
        assert_eq!(s.entries().iter().filter(|e| e.baseline_only).count(), 1);
        assert!(
            s.entries()
                .iter()
                .any(|e| e.entry_id == "t1" && !e.baseline_only)
        );
        // The compaction was persisted, not just in-memory.
        let reopened = WorkspaceCheckpointStore::open(&cp, dir.path()).unwrap();
        assert_eq!(
            reopened
                .entries()
                .iter()
                .filter(|e| e.baseline_only)
                .count(),
            1
        );
    }

    #[test]
    fn checkpoint_restore_refuses_cleanly_when_a_blob_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let tracked = vec!["a.txt".to_string(), "b.txt".to_string()];

        fs::write(&a, b"AAA").unwrap();
        fs::write(&b, b"BBB").unwrap();
        let mut s = store(dir.path());
        s.capture("t1", &tracked, None).unwrap();
        // A later turn changes both; the tree now matches t2 (no drift).
        fs::write(&a, b"AAA2").unwrap();
        fs::write(&b, b"BBB2").unwrap();
        s.capture("t2", &tracked, None).unwrap();

        // Corrupt the store: drop t1's `a.txt` blob (content "AAA").
        let blobs = dir.path().join(".checkpoints").join("blobs");
        for entry in fs::read_dir(&blobs).unwrap().filter_map(Result::ok) {
            if fs::read(entry.path()).unwrap() == b"AAA" {
                fs::remove_file(entry.path()).unwrap();
            }
        }

        let err = s.restore("t1").unwrap_err();
        assert!(matches!(err, CheckpointError::MissingBlob(_)), "{err:?}");
        // The refusal is a true no-op: b.txt was NOT half-restored to BBB.
        assert_eq!(fs::read(&b).unwrap(), b"BBB2");
        assert_eq!(fs::read(&a).unwrap(), b"AAA2");
    }

    #[test]
    fn checkpoint_restore_refuses_when_working_tree_drifted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.rs");
        let tracked = vec!["code.rs".to_string()];

        fs::write(&file, b"v1").unwrap();
        let mut s = store(dir.path());
        s.capture("turn1", &tracked, None).unwrap();
        fs::write(&file, b"v2").unwrap();
        s.capture("turn2", &tracked, None).unwrap();

        // An external edit after the last capture.
        fs::write(&file, b"v3-user-edit").unwrap();

        let err = s.restore("turn1").unwrap_err();
        assert!(matches!(
            err,
            CheckpointError::WorkingTreeDrifted { path } if path == "code.rs"
        ));
        // Refusal must leave the working tree untouched.
        assert_eq!(fs::read(&file).unwrap(), b"v3-user-edit");
    }

    #[test]
    fn checkpoint_manifest_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let tracked = vec!["a.txt".to_string(), "missing.txt".to_string()];

        {
            let mut s = store(dir.path());
            s.capture("turn1", &tracked, Some("anchor".to_string()))
                .unwrap();
        }

        // Reopen from disk; the manifest must survive verbatim.
        let reopened = store(dir.path());
        assert_eq!(reopened.entries().len(), 1);
        let entry = &reopened.entries()[0];
        assert_eq!(entry.entry_id, "turn1");
        assert_eq!(entry.label.as_deref(), Some("anchor"));
        assert_eq!(entry.files["missing.txt"], FileState::Absent);
        assert!(matches!(entry.files["a.txt"], FileState::Blob(_)));
    }

    #[test]
    fn checkpoint_capture_records_absent_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = store(dir.path());
        s.capture("turn1", &["ghost.rs".to_string()], None).unwrap();
        assert_eq!(s.entries()[0].files["ghost.rs"], FileState::Absent);
    }

    #[test]
    fn checkpoint_restore_unknown_entry_is_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        let err = s.restore("nope").unwrap_err();
        assert!(matches!(err, CheckpointError::UnknownCheckpoint(id) if id == "nope"));
    }
}
