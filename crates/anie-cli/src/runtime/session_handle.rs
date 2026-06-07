//! `SessionHandle` — owner of the current session file and related paths.
//!
//! Wraps `SessionManager` plus the directory context it lives in
//! (`sessions_dir` for siblings, `cwd` for new sessions). The
//! controller used to carry these three fields separately; grouping
//! them clarifies which methods are "pure session" (fork, diff,
//! list, build_context) versus coordination across session + config
//! (set_model, apply_session_overrides — those stay on
//! ControllerState).

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use anie_protocol::{ContentBlock, Message};
use anie_session::{
    RestorePlan, SessionContext, SessionEntry, SessionInfo, SessionManager,
    WorkspaceCheckpointStore,
};

/// A selectable rewind anchor: a captured user turn or a named
/// `/checkpoint`. Carries the session entry id to rewind to plus a
/// short human label for the listing.
pub(crate) struct RewindPoint {
    /// Session entry id to pass to [`SessionHandle::rewind_to`].
    pub entry_id: String,
    /// User-supplied label, set by `/checkpoint <name>`.
    pub label: Option<String>,
    /// First line of the user message at this entry (empty for
    /// non-message anchors).
    pub summary: String,
}

/// Owns the currently-active session plus its directory context.
pub(crate) struct SessionHandle {
    session: SessionManager,
    sessions_dir: PathBuf,
    cwd: PathBuf,
}

impl SessionHandle {
    /// Wrap an already-opened `SessionManager`.
    pub(crate) fn from_manager(
        session: SessionManager,
        sessions_dir: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        Self {
            session,
            sessions_dir,
            cwd,
        }
    }

    /// Path to the directory holding all session JSONL files.
    #[allow(dead_code)] // reserved for future session-picker work
    pub(crate) fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Current working directory associated with this session.
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Borrow the underlying session manager for read-only direct
    /// operations. Prefer the higher-level methods on this handle
    /// when possible.
    #[allow(dead_code)] // reserved for future session-picker work
    pub(crate) fn inner(&self) -> &SessionManager {
        &self.session
    }

    /// Mutable access to the underlying session manager. Required for
    /// append operations and fork; prefer the higher-level methods on
    /// this handle when possible.
    pub(crate) fn inner_mut(&mut self) -> &mut SessionManager {
        &mut self.session
    }

    /// Session id (file stem).
    pub(crate) fn id(&self) -> &str {
        self.session.id()
    }

    /// Reconstruct the active-branch context from the on-disk log.
    pub(crate) fn context(&self) -> SessionContext {
        self.session.build_context()
    }

    /// Return the active-branch messages with one entry optionally
    /// filtered out. Used to avoid replaying the freshly-appended
    /// prompt to the model via context when it's already in
    /// `prompts`.
    pub(crate) fn context_without_entry(&self, entry_id: Option<&str>) -> Vec<Message> {
        self.session
            .build_context()
            .messages
            .into_iter()
            .filter(|message| entry_id.is_none_or(|excluded| message.entry_id != excluded))
            .map(|message| message.message)
            .collect()
    }

    /// Estimate token usage for the active-branch context.
    pub(crate) fn estimated_context_tokens(&self) -> u64 {
        self.session.estimate_context_tokens()
    }

    /// List all sessions in `sessions_dir`, newest first.
    pub(crate) fn list(&self) -> Result<Vec<SessionInfo>> {
        SessionManager::list_sessions(&self.sessions_dir)
    }

    /// Replace the active session with a newly-created sibling.
    /// Caller should follow up with config/runtime-state
    /// reconciliation.
    pub(crate) fn start_new(&mut self) -> Result<()> {
        self.session = SessionManager::new_session(&self.sessions_dir, &self.cwd)?;
        Ok(())
    }

    /// Switch to an existing session by id. Errors with
    /// `SessionError::AlreadyOpen` (wrapped in anyhow) if the target
    /// is already held by another process.
    pub(crate) fn switch_to(&mut self, session_id: &str) -> Result<()> {
        let path = self.sessions_dir.join(format!("{session_id}.jsonl"));
        let session = SessionManager::open_session(&path)
            .with_context(|| format!("failed to open session {session_id}"))?;
        self.session = session;
        Ok(())
    }

    /// Fork the current session into a new child. Returns the child
    /// session id after making the fork active.
    pub(crate) fn fork(&mut self) -> Result<String> {
        let child = self.session.fork_to_child_session(&self.sessions_dir)?;
        let child_id = child.id().to_string();
        self.session = child;
        Ok(child_id)
    }

    /// Render a human-readable diff of files modified by tool calls
    /// in this session's active branch.
    pub(crate) fn diff(&self) -> String {
        let Some(leaf_id) = self.session.leaf_id() else {
            return "No file changes in this session yet.".into();
        };

        let branch = self.session.get_branch(leaf_id);
        let mut modified_files = Vec::new();
        let mut seen_paths = HashSet::new();
        let mut diff_sections = Vec::new();

        for entry in branch {
            let anie_session::SessionEntry::Message { message, .. } = entry else {
                continue;
            };
            let Message::ToolResult(tool_result) = message else {
                continue;
            };
            if tool_result.tool_name != "edit" && tool_result.tool_name != "write" {
                continue;
            }

            if let Some(path) = tool_result
                .details
                .get("path")
                .and_then(serde_json::Value::as_str)
                && seen_paths.insert(path.to_string())
            {
                modified_files.push(format!("- {} ({})", path, tool_result.tool_name));
            }

            if tool_result.tool_name == "edit"
                && let Some(diff) = tool_result
                    .details
                    .get("diff")
                    .and_then(serde_json::Value::as_str)
            {
                let title = tool_result
                    .details
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("[unknown path]");
                diff_sections.push(format!("--- {title} ---\n{diff}"));
            }
        }

        if modified_files.is_empty() {
            return "No file changes in this session yet.".into();
        }

        let mut output = String::from("Files modified in this session:\n");
        output.push_str(&modified_files.join("\n"));
        if !diff_sections.is_empty() {
            output.push_str("\n\nEdit diffs:\n\n");
            output.push_str(&diff_sections.join("\n\n"));
        }
        output
    }

    /// Flush buffered session writes to disk.
    pub(crate) fn flush(&mut self) -> Result<()> {
        self.session.flush()
    }

    /// Sidecar directory holding this session's checkpoint store.
    fn checkpoint_dir(&self) -> PathBuf {
        self.sessions_dir
            .join(format!("{}.checkpoints", self.session.id()))
    }

    /// Open (or create) the working-tree checkpoint store for the
    /// current session, with tracked paths resolved against `cwd`.
    fn open_checkpoint_store(&self) -> Result<WorkspaceCheckpointStore> {
        WorkspaceCheckpointStore::open(self.checkpoint_dir(), &self.cwd)
            .context("failed to open checkpoint store")
    }

    /// Snapshot the working tree at a session entry, over the set of
    /// files the agent has written/edited on the active branch. Called
    /// at each user-turn boundary (and by `/checkpoint` with a label).
    /// Content addressing makes an unchanged file nearly free.
    pub(crate) fn capture_checkpoint(
        &mut self,
        entry_id: &str,
        label: Option<String>,
    ) -> Result<()> {
        let tracked = self.session.tracked_modified_files();
        let mut store = self.open_checkpoint_store()?;
        store
            .capture(entry_id, &tracked, label)
            .context("failed to capture checkpoint")
    }

    /// Rewind both the working tree and the conversation to a prior
    /// entry: restore tracked files to their captured state, then
    /// re-point the active leaf via the existing append-only
    /// [`SessionManager::fork`]. The working tree is restored first, so
    /// a drift refusal leaves the conversation untouched. Returns the
    /// [`RestorePlan`] describing what changed on disk.
    pub(crate) fn rewind_to(&mut self, entry_id: &str) -> Result<RestorePlan> {
        let store = self.open_checkpoint_store()?;
        // Restore (and drift-check) before mutating session state, so a
        // refusal is a clean no-op.
        let plan = store.restore(entry_id)?;
        self.session.fork(entry_id)?;
        Ok(plan)
    }

    /// Record a checkpoint at the current leaf with an optional label
    /// (the `/checkpoint [name]` anchor). Returns the entry id anchored,
    /// or `None` when the session has no leaf yet.
    pub(crate) fn capture_named_checkpoint(
        &mut self,
        label: Option<String>,
    ) -> Result<Option<String>> {
        let Some(leaf) = self.session.leaf_id().map(str::to_string) else {
            return Ok(None);
        };
        self.capture_checkpoint(&leaf, label)?;
        Ok(Some(leaf))
    }

    /// The recorded rewind anchors, oldest first, each annotated with
    /// the first line of its user turn for display.
    pub(crate) fn rewind_points(&self) -> Result<Vec<RewindPoint>> {
        let store = self.open_checkpoint_store()?;
        let points = store
            .entries()
            .iter()
            .map(|entry| RewindPoint {
                entry_id: entry.entry_id.clone(),
                label: entry.label.clone(),
                summary: self.entry_summary(&entry.entry_id),
            })
            .collect();
        Ok(points)
    }

    /// First line of the user message stored at `entry_id`, or empty
    /// for anchors that don't point at a user turn.
    fn entry_summary(&self, entry_id: &str) -> String {
        let Some(SessionEntry::Message {
            message: Message::User(user),
            ..
        }) = self.session.get_entry(entry_id)
        else {
            return String::new();
        };
        user.content
            .iter()
            .find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.lines().next().unwrap_or("").to_string()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use anie_protocol::{AssistantMessage, ContentBlock, StopReason, ToolCall, Usage, UserMessage};
    use anie_session::CheckpointError;
    use serde_json::json;

    fn handle(dir: &Path) -> SessionHandle {
        let sessions_dir = dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session = SessionManager::new_session(&sessions_dir, dir).unwrap();
        SessionHandle::from_manager(session, sessions_dir, dir.to_path_buf())
    }

    fn user(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: 1,
        })
    }

    /// An assistant turn whose tool calls touch `(tool_name, path)`.
    fn assistant_tool_calls(calls: &[(&str, &str)]) -> Message {
        let content = calls
            .iter()
            .map(|(name, path)| {
                ContentBlock::ToolCall(ToolCall {
                    id: format!("call-{name}-{path}"),
                    name: (*name).to_string(),
                    arguments: json!({ "path": path }),
                })
            })
            .collect();
        Message::Assistant(AssistantMessage {
            content,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            timestamp: 1,
            reasoning_details: None,
        })
    }

    #[test]
    fn turn_boundary_captures_tracked_files_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), b"written").unwrap();
        std::fs::write(dir.path().join("b.rs"), b"edited").unwrap();

        let mut h = handle(dir.path());
        h.inner_mut().append_message(&user("go")).unwrap();
        h.inner_mut()
            .append_message(&assistant_tool_calls(&[
                ("read", "c.rs"),
                ("write", "a.rs"),
                ("edit", "b.rs"),
            ]))
            .unwrap();

        h.capture_checkpoint("turn1", None).unwrap();

        let store = h.open_checkpoint_store().unwrap();
        let files = &store.entries()[0].files;
        // write/edit paths are tracked; the read-only path is not.
        assert!(files.contains_key("a.rs"));
        assert!(files.contains_key("b.rs"));
        assert!(!files.contains_key("c.rs"));
    }

    #[test]
    fn rewind_to_restores_working_tree_and_forks_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("code.rs");
        std::fs::write(&file, b"v1").unwrap();

        let mut h = handle(dir.path());
        let uid1 = h.inner_mut().append_message(&user("turn1")).unwrap();
        h.inner_mut()
            .append_message(&assistant_tool_calls(&[("write", "code.rs")]))
            .unwrap();
        h.capture_checkpoint(&uid1, None).unwrap();

        std::fs::write(&file, b"v2").unwrap();
        h.inner_mut().append_message(&user("turn2")).unwrap();
        h.inner_mut()
            .append_message(&assistant_tool_calls(&[("edit", "code.rs")]))
            .unwrap();
        h.capture_checkpoint("turn2", None).unwrap();

        let plan = h.rewind_to(&uid1).unwrap();
        assert_eq!(plan.written, vec!["code.rs".to_string()]);
        assert_eq!(std::fs::read(&file).unwrap(), b"v1");
        // The conversation leaf is re-pointed at the rewound turn.
        assert_eq!(h.inner().leaf_id(), Some(uid1.as_str()));
    }

    #[test]
    fn rewind_to_unknown_entry_returns_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = handle(dir.path());
        let err = h.rewind_to("ghost").unwrap_err();
        assert!(matches!(
            err.downcast_ref::<CheckpointError>(),
            Some(CheckpointError::UnknownCheckpoint(id)) if id == "ghost"
        ));
    }
}
