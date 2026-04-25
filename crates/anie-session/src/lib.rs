//! Session persistence and context compaction for anie-rs.
//!
//! ## Concurrency
//!
//! A session file is opened with an exclusive advisory file lock
//! (via `fs4`). A second attempt to open the same file returns
//! `SessionError::AlreadyOpen`. On platforms that don't support
//! advisory locks (some network filesystems), the lock attempt is
//! a no-op and a warning is logged.
//!
//! Within a single process, a `SessionManager` owns its file; there
//! is no cross-task sharing. Concurrent writes from multiple tasks
//! in the same process are also undefined — clone the session via
//! `fork_to_child_session` if you need a second writer.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tracing::warn;
use uuid::Uuid;

/// Domain errors returned by the session subsystem.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Another process or in-process owner holds the session file.
    #[error("session file {0} is already open by another writer")]
    AlreadyOpen(PathBuf),
}

/// Try to acquire an exclusive advisory lock on the given file.
///
/// Returns `Ok(true)` when the lock was acquired, `Ok(false)` when
/// the filesystem does not support locking (we log a warning and
/// proceed without a lock), and `Err` when another writer holds it.
fn try_acquire_session_lock(file: &File, path: &Path) -> Result<bool, SessionError> {
    match FileExt::try_lock_exclusive(file) {
        Ok(true) => Ok(true),
        Ok(false) => Err(SessionError::AlreadyOpen(path.to_path_buf())),
        Err(error) => {
            // Best-effort: some filesystems (older NFS, WSL edge cases)
            // return errors instead of `Ok(false)`. Log and proceed
            // without the lock rather than blocking the user.
            warn!(
                path = %path.display(),
                %error,
                "filesystem does not support advisory file locking; \
                 concurrent writers will not be detected"
            );
            Ok(false)
        }
    }
}

use anie_protocol::{ContentBlock, Message, UserMessage};
use anie_provider::ThinkingLevel;

/// Current session-file schema version. Bump every time a change is
/// made that affects how older binaries should interpret the file, and
/// update the table in docs/api_integrity_plans/05_session_schema_migration.md.
///
/// | Version | Change                                                |
/// |---------|-------------------------------------------------------|
/// | 1       | Baseline.                                             |
/// | 2       | `ContentBlock::Thinking.signature` optional field     |
/// |         | + `ContentBlock::RedactedThinking` variant. Both      |
/// |         | forward- and backward-compatible via serde defaults.  |
/// | 3       | `AssistantMessage.reasoning_details` optional field   |
/// |         | for OpenRouter encrypted-reasoning replay. Stored as  |
/// |         | opaque JSON; forward- and backward-compatible via     |
/// |         | serde defaults.                                       |
/// | 4       | `SessionEntry::Compaction.details` optional field     |
/// |         | carrying `CompactionDetails` (read / modified file    |
/// |         | lists, deduplicated). Forward- and backward-          |
/// |         | compatible via serde defaults. Mirrors pi's           |
/// |         | `CompactionDetails` shape at                          |
/// |         | `packages/coding-agent/src/core/compaction/           |
/// |         | compaction.ts:~33`.                                   |
pub const CURRENT_SESSION_SCHEMA_VERSION: u32 = 4;

/// Session-file header. Always the first line in a session JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHeader {
    /// Discriminator. Always `session`.
    #[serde(rename = "type")]
    pub entry_type: String,
    /// File-format version.
    pub version: u32,
    /// Session identifier.
    pub id: String,
    /// Creation timestamp.
    pub timestamp: String,
    /// Working directory associated with the session.
    pub cwd: String,
    /// Optional parent session ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// Base fields shared by all session entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntryBase {
    /// Entry identifier.
    pub id: String,
    /// Parent entry on the active branch.
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    /// Entry timestamp.
    pub timestamp: String,
}

/// All append-only session entry variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum SessionEntry {
    /// A conversation message.
    #[serde(rename = "message")]
    Message {
        /// Shared entry metadata.
        #[serde(flatten)]
        base: EntryBase,
        /// Stored message payload.
        message: Message,
    },
    /// A context-compaction checkpoint.
    #[serde(rename = "compaction")]
    Compaction {
        /// Shared entry metadata.
        #[serde(flatten)]
        base: EntryBase,
        /// Human-readable summary of compacted messages.
        summary: String,
        /// Estimated tokens before compaction.
        tokens_before: u64,
        /// Entry ID of the first kept verbatim message.
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: String,
        /// Optional structured details extracted from the
        /// discarded interval — read / modified file paths.
        /// Mirrors pi's `CompactionDetails` shape; `None` on
        /// pre-v4 sessions or when no tracked tools were called.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<CompactionDetails>,
    },
    /// A persisted model selection change.
    #[serde(rename = "model_change")]
    ModelChange {
        /// Shared entry metadata.
        #[serde(flatten)]
        base: EntryBase,
        /// Provider name.
        provider: String,
        /// Model identifier.
        model: String,
    },
    /// A persisted thinking-level change.
    #[serde(rename = "thinking_change")]
    ThinkingChange {
        /// Shared entry metadata.
        #[serde(flatten)]
        base: EntryBase,
        /// Requested thinking level.
        level: ThinkingLevel,
    },
    /// Optional user-facing label metadata.
    #[serde(rename = "label")]
    Label {
        /// Shared entry metadata.
        #[serde(flatten)]
        base: EntryBase,
        /// Entry being labeled.
        target_id: String,
        /// Optional label text.
        label: Option<String>,
    },
}

impl SessionEntry {
    /// Access the shared entry metadata.
    #[must_use]
    pub fn base(&self) -> &EntryBase {
        match self {
            Self::Message { base, .. }
            | Self::Compaction { base, .. }
            | Self::ModelChange { base, .. }
            | Self::ThinkingChange { base, .. }
            | Self::Label { base, .. } => base,
        }
    }
}

/// A context message paired with the session entry that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContextMessage {
    /// Source entry identifier.
    pub entry_id: String,
    /// Canonical message payload.
    pub message: Message,
}

/// Reconstructed session state for a single active branch.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionContext {
    /// Canonical message sequence.
    pub messages: Vec<SessionContextMessage>,
    /// Most recent persisted thinking level, if any.
    pub thinking_level: Option<ThinkingLevel>,
    /// Most recent persisted model selection `(provider, model_id)`, if any.
    pub model: Option<(String, String)>,
    /// Structured details from the most recent compaction on
    /// this branch, if one has run and carried a payload.
    /// Mirrors the typed `details` field on the corresponding
    /// `SessionEntry::Compaction` — callers that need
    /// programmatic access to file-op history (e.g. to seed a
    /// "recently-touched files" hint on resume) can read it
    /// without re-walking entries. `None` when the session has
    /// no compactions or when the most recent compaction had no
    /// tracked tool calls.
    pub compaction_details: Option<CompactionDetails>,
}

impl SessionContext {
    /// Create an empty session context.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            messages: Vec::new(),
            thinking_level: None,
            model: None,
            compaction_details: None,
        }
    }
}

/// Structured details about a compacted interval.
///
/// Mirrors pi's `CompactionDetails` shape at
/// `packages/coding-agent/src/core/compaction/compaction.ts:~33`
/// — two string vectors, deliberately minimal. Bash commands,
/// file sizes, exit codes, etc. are explicitly NOT tracked here;
/// pi's 2-field shape is what consumers actually need.
///
/// **anie-specific (not in pi):** none. Deduplication is
/// handled by `extract_compaction_details` during construction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CompactionDetails {
    /// Paths read via tracked tool calls during the summarized
    /// interval. Dropped from serialization when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_files: Vec<String>,
    /// Paths written or edited via tracked tool calls during
    /// the summarized interval. Dropped from serialization when
    /// empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<String>,
}

impl CompactionDetails {
    /// Whether both vectors are empty. Used by the compaction
    /// caller to decide `Option<CompactionDetails>`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.read_files.is_empty() && self.modified_files.is_empty()
    }
}

/// Walk a discarded message sequence and extract file paths
/// from `read` / `write` / `edit` tool calls. Paths are
/// deduplicated in first-seen order.
///
/// Matches pi's extraction logic: only the canonical `read`,
/// `write`, and `edit` tool names are tracked. Custom or
/// user-registered tools with different names are intentionally
/// not picked up — correctness beats breadth here.
#[must_use]
pub fn extract_compaction_details(messages: &[Message]) -> CompactionDetails {
    let mut read_files: Vec<String> = Vec::new();
    let mut modified_files: Vec<String> = Vec::new();
    for message in messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            let Some(path) = tool_call_path(&call.arguments) else {
                continue;
            };
            match call.name.as_str() {
                "read" => push_unique(&mut read_files, path),
                "write" | "edit" => push_unique(&mut modified_files, path),
                _ => {}
            }
        }
    }
    CompactionDetails {
        read_files,
        modified_files,
    }
}

/// Extract the file path from a tool-call argument object.
/// Handles `path` (canonical) and `file_path` (alternative
/// convention seen in some provider / third-party tool
/// definitions).
fn tool_call_path(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("path")
        .or_else(|| arguments.get("file_path"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn push_unique(vec: &mut Vec<String>, value: String) {
    if !vec.iter().any(|existing| existing == &value) {
        vec.push(value);
    }
}

/// Append file-op details to a summary as XML-like tag blocks.
/// The summarizer LLM sees the prose; the tags give resumed
/// sessions a grep-able record of exact paths inside the
/// compacted summary text. pi does this so file paths survive
/// even when the prose doesn't mention them explicitly.
#[must_use]
pub fn append_details_to_summary(summary: &str, details: &CompactionDetails) -> String {
    if details.is_empty() {
        return summary.to_string();
    }
    let mut out = summary.trim_end().to_string();
    if !details.read_files.is_empty() {
        out.push_str("\n\n<read-files>\n");
        for path in &details.read_files {
            out.push_str(path);
            out.push('\n');
        }
        out.push_str("</read-files>");
    }
    if !details.modified_files.is_empty() {
        out.push_str("\n\n<modified-files>\n");
        for path in &details.modified_files {
            out.push_str(path);
            out.push('\n');
        }
        out.push_str("</modified-files>");
    }
    out
}

/// Summary metadata returned after a successful compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionResult {
    /// Generated summary text.
    pub summary: String,
    /// Estimated tokens before compaction.
    pub tokens_before: u64,
    /// Entry ID of the first kept message.
    pub first_kept_entry_id: String,
    /// Count of compacted messages.
    pub messages_discarded: usize,
}

/// Session-compaction thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    /// Model context window.
    pub context_window: u64,
    /// Reserved space that should remain available.
    pub reserve_tokens: u64,
    /// Recent token budget kept verbatim.
    pub keep_recent_tokens: u64,
}

/// Summarizes a range of messages into compact text for context
/// compaction.
///
/// The session crate no longer knows how to talk to LLM providers —
/// callers (typically the CLI's `CompactionStrategy`) implement this
/// trait against their `ProviderRegistry` + resolver + selected
/// model. Keeping the contract narrow lets the session crate stay
/// provider-agnostic.
#[async_trait]
pub trait MessageSummarizer: Send + Sync {
    /// Produce a summary of `messages`.
    ///
    /// - `messages` is the conversation slice to be compacted.
    /// - `existing_summary` is the most recent prior compaction
    ///   summary on the active branch, if any. Implementations
    ///   should merge with rather than replace it.
    ///
    /// Returns the new summary text. Must return an error if the
    /// summary is empty or the underlying request fails.
    async fn summarize(
        &self,
        messages: &[Message],
        existing_summary: Option<&str>,
    ) -> Result<String>;
}

/// Listed session metadata for `/session list` and resume flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Session file path.
    pub path: PathBuf,
    /// Session identifier.
    pub id: String,
    /// Session working directory.
    pub cwd: String,
    /// Header timestamp.
    pub created: String,
    /// Last modified filesystem timestamp.
    pub modified: SystemTime,
    /// Number of stored message entries.
    pub message_count: u32,
    /// First user-authored message, when available.
    pub first_message: String,
}

/// Append-only JSONL session manager.
pub struct SessionManager {
    path: PathBuf,
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    /// Plan 03 PR-A: `by_id` is the single membership index.
    /// The previous separate `id_set: HashSet<String>` was
    /// redundant — every insert/remove had to mirror both.
    /// Removed so contains-check flows through
    /// `by_id.contains_key(...)` only.
    by_id: HashMap<String, usize>,
    leaf_id: Option<String>,
    file_handle: File,
}

impl SessionManager {
    /// Create a new session file inside `sessions_dir`.
    pub fn new_session(sessions_dir: &Path, cwd: &Path) -> Result<Self> {
        Self::new_session_with_parent(sessions_dir, cwd, None)
    }

    fn new_session_with_parent(
        sessions_dir: &Path,
        cwd: &Path,
        parent_session: Option<String>,
    ) -> Result<Self> {
        fs::create_dir_all(sessions_dir)
            .with_context(|| format!("failed to create {}", sessions_dir.display()))?;

        let mut existing = HashSet::new();
        for entry in fs::read_dir(sessions_dir)
            .with_context(|| format!("failed to read {}", sessions_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("jsonl")
                && let Some(stem) = path.file_stem().and_then(|value| value.to_str())
            {
                existing.insert(stem.to_string());
            }
        }

        let session_id = generate_unique_id(|id| existing.contains(id));
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let header = SessionHeader {
            entry_type: "session".into(),
            version: CURRENT_SESSION_SCHEMA_VERSION,
            id: session_id,
            timestamp: now_iso8601()?,
            cwd: cwd.display().to_string(),
            parent_session,
        };

        let mut file_handle = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        writeln!(file_handle, "{}", serde_json::to_string(&header)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
        file_handle
            .flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;

        let file_handle = OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to reopen {}", path.display()))?;
        try_acquire_session_lock(&file_handle, &path)?;

        Ok(Self {
            path,
            header,
            entries: Vec::new(),
            by_id: HashMap::new(),
            leaf_id: None,
            file_handle,
        })
    }

    /// Open an existing session file and rebuild its in-memory indexes.
    pub fn open_session(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        let (header, entries) = parse_session_file(&content)?;

        // Forward-compatibility gate: older binaries must not
        // silently load sessions written by newer ones — that would
        // either lose state (new fields serde-ignored) or panic on
        // unknown variant tags. Bail with a clear error instead.
        // Older-version files (< CURRENT) load normally; serde
        // defaults cover any fields they lack.
        if header.version > CURRENT_SESSION_SCHEMA_VERSION {
            anyhow::bail!(
                "session file {} was written with schema version {} \
                 but this binary only supports up to version {} — \
                 upgrade anie to continue. See \
                 docs/api_integrity_plans/05_session_schema_migration.md.",
                path.display(),
                header.version,
                CURRENT_SESSION_SCHEMA_VERSION,
            );
        }

        let mut by_id = HashMap::new();
        let mut leaf_id = None;

        for (index, entry) in entries.iter().enumerate() {
            let id = entry.base().id.clone();
            by_id.insert(id.clone(), index);
            leaf_id = Some(id);
        }

        let file_handle = OpenOptions::new()
            .append(true)
            .open(path)
            .with_context(|| format!("failed to reopen {}", path.display()))?;
        try_acquire_session_lock(&file_handle, path)?;

        Ok(Self {
            path: path.to_path_buf(),
            header,
            entries,
            by_id,
            leaf_id,
            file_handle,
        })
    }

    /// Return the session file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush buffered session writes to disk.
    pub fn flush(&mut self) -> Result<()> {
        self.file_handle
            .flush()
            .with_context(|| format!("failed to flush {}", self.path.display()))
    }

    /// Return the session identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.header.id
    }

    /// Access the session header.
    #[must_use]
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Return the current active leaf entry ID.
    #[must_use]
    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// Return all stored entries.
    #[must_use]
    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    /// Look up a specific entry by ID.
    #[must_use]
    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }

    /// Point the active branch at an earlier entry to allow a new branch.
    pub fn fork(&mut self, from_entry_id: &str) -> Result<()> {
        if !self.by_id.contains_key(from_entry_id) {
            return Err(anyhow!("entry {from_entry_id} was not found"));
        }
        self.leaf_id = Some(from_entry_id.to_string());
        Ok(())
    }

    /// Create a new child session file seeded with the current active branch.
    pub fn fork_to_child_session(&self, sessions_dir: &Path) -> Result<Self> {
        let mut child = Self::new_session_with_parent(
            sessions_dir,
            Path::new(&self.header.cwd),
            Some(self.header.id.clone()),
        )?;

        if let Some(leaf_id) = self.leaf_id() {
            let entries = self
                .get_branch(leaf_id)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            child.add_entries(entries)?;
        }

        Ok(child)
    }

    /// Append already-constructed entries to the current branch.
    pub fn add_entries(&mut self, entries: Vec<SessionEntry>) -> Result<Vec<String>> {
        let mut appended_ids = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(parent_id) = &entry.base().parent_id
                && !self.by_id.contains_key(parent_id)
            {
                return Err(anyhow!("parent ID {parent_id} was not found"));
            }
            let line = serde_json::to_string(&entry)?;
            writeln!(self.file_handle, "{line}")
                .with_context(|| format!("failed to append to {}", self.path.display()))?;
            self.file_handle
                .flush()
                .with_context(|| format!("failed to flush {}", self.path.display()))?;

            let index = self.entries.len();
            let id = entry.base().id.clone();
            self.by_id.insert(id.clone(), index);
            self.leaf_id = Some(id.clone());
            self.entries.push(entry);
            appended_ids.push(id);
        }
        Ok(appended_ids)
    }

    /// Append a single message at the current leaf.
    pub fn append_message(&mut self, message: &Message) -> Result<String> {
        let entry = SessionEntry::Message {
            base: EntryBase {
                id: self.generate_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: now_iso8601()?,
            },
            message: message.clone(),
        };
        let mut ids = self.add_entries(vec![entry])?;
        ids.pop()
            .ok_or_else(|| anyhow::anyhow!("message append returned no id"))
    }

    /// Append multiple messages in sequence at the current leaf.
    pub fn append_messages(&mut self, messages: &[Message]) -> Result<Vec<String>> {
        let mut entries = Vec::with_capacity(messages.len());
        let mut parent_id = self.leaf_id.clone();
        for message in messages {
            let id = self.generate_id();
            entries.push(SessionEntry::Message {
                base: EntryBase {
                    id: id.clone(),
                    parent_id: parent_id.clone(),
                    timestamp: now_iso8601()?,
                },
                message: message.clone(),
            });
            parent_id = Some(id);
        }
        self.add_entries(entries)
    }

    /// Persist a model-selection change.
    pub fn append_model_change(&mut self, provider: &str, model: &str) -> Result<String> {
        let entry = SessionEntry::ModelChange {
            base: EntryBase {
                id: self.generate_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: now_iso8601()?,
            },
            provider: provider.to_string(),
            model: model.to_string(),
        };
        let mut ids = self.add_entries(vec![entry])?;
        ids.pop()
            .ok_or_else(|| anyhow::anyhow!("model change append returned no id"))
    }

    /// Persist a thinking-level change.
    pub fn append_thinking_change(&mut self, level: ThinkingLevel) -> Result<String> {
        let entry = SessionEntry::ThinkingChange {
            base: EntryBase {
                id: self.generate_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: now_iso8601()?,
            },
            level,
        };
        let mut ids = self.add_entries(vec![entry])?;
        ids.pop()
            .ok_or_else(|| anyhow::anyhow!("thinking change append returned no id"))
    }

    /// Return the active branch from root to leaf.
    #[must_use]
    pub fn get_branch(&self, leaf_id: &str) -> Vec<&SessionEntry> {
        let mut branch = Vec::new();
        let mut current_id = Some(leaf_id.to_string());

        while let Some(id) = current_id {
            let Some(index) = self.by_id.get(&id) else {
                break;
            };
            let entry = &self.entries[*index];
            branch.push(entry);
            current_id = entry.base().parent_id.clone();
        }

        branch.reverse();
        branch
    }

    /// Rebuild the canonical context for the current active branch.
    #[must_use]
    pub fn build_context(&self) -> SessionContext {
        let Some(leaf_id) = &self.leaf_id else {
            return SessionContext::empty();
        };

        let branch = self.get_branch(leaf_id);
        let latest_compaction = branch.iter().rev().find_map(|entry| match entry {
            SessionEntry::Compaction {
                summary,
                first_kept_entry_id,
                details,
                ..
            } => Some((
                summary.clone(),
                first_kept_entry_id.clone(),
                details.clone(),
            )),
            _ => None,
        });

        let mut thinking_level = None;
        let mut model = None;
        for entry in &branch {
            match entry {
                SessionEntry::ThinkingChange { level, .. } => thinking_level = Some(*level),
                SessionEntry::ModelChange {
                    provider,
                    model: model_id,
                    ..
                } => model = Some((provider.clone(), model_id.clone())),
                _ => {}
            }
        }

        let mut messages = Vec::new();
        let mut keep_messages = latest_compaction.is_none();
        if let Some((summary, _, _)) = &latest_compaction {
            messages.push(SessionContextMessage {
                entry_id: format!("summary:{}", self.header.id),
                message: Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: format!("[Previous conversation summary]\n\n{summary}"),
                    }],
                    timestamp: 0,
                }),
            });
        }

        for entry in &branch {
            if let Some((_, first_kept_entry_id, _)) = &latest_compaction
                && !keep_messages
            {
                if entry.base().id == *first_kept_entry_id {
                    keep_messages = true;
                } else {
                    continue;
                }
            }

            if !keep_messages {
                continue;
            }

            if let SessionEntry::Message { base, message } = entry {
                messages.push(SessionContextMessage {
                    entry_id: base.id.clone(),
                    message: message.clone(),
                });
            }
        }

        let compaction_details = latest_compaction.and_then(|(_, _, details)| details);

        SessionContext {
            messages,
            thinking_level,
            model,
            compaction_details,
        }
    }

    /// Estimate token usage for the current active branch without
    /// materializing a full `SessionContext`. Produces the same total
    /// as `estimate_context_tokens(&self.build_context().messages)`
    /// but avoids cloning every message.
    #[must_use]
    pub fn estimate_context_tokens(&self) -> u64 {
        let Some(leaf_id) = self.leaf_id.as_deref() else {
            return 0;
        };

        let branch = self.get_branch(leaf_id);
        let latest_compaction = branch.iter().rev().find_map(|entry| match entry {
            SessionEntry::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => Some((summary.as_str(), first_kept_entry_id.as_str())),
            _ => None,
        });

        let mut total: u64 = 0;
        if let Some((summary, _)) = latest_compaction {
            // Mirror the synthetic summary message produced by
            // `build_context()` so the counts match exactly.
            let prefix_len = "[Previous conversation summary]\n\n".len() as u64;
            total = total.saturating_add((prefix_len + summary.len() as u64) / 4);
        }

        let mut keep_messages = latest_compaction.is_none();
        for entry in branch {
            if let Some((_, first_kept_entry_id)) = latest_compaction
                && !keep_messages
            {
                if entry.base().id == first_kept_entry_id {
                    keep_messages = true;
                } else {
                    continue;
                }
            }
            if !keep_messages {
                continue;
            }
            if let SessionEntry::Message { message, .. } = entry {
                total = total.saturating_add(estimate_tokens(message));
            }
        }

        total
    }

    /// Compact the session if the current context exceeds the
    /// configured threshold. The summarizer is only invoked when
    /// compaction actually runs.
    pub async fn auto_compact(
        &mut self,
        config: &CompactionConfig,
        summarizer: &dyn MessageSummarizer,
    ) -> Result<Option<CompactionResult>> {
        let tokens_before = self.estimate_context_tokens();
        let threshold = config.context_window.saturating_sub(config.reserve_tokens);
        if tokens_before <= threshold {
            return Ok(None);
        }
        self.compact_internal(tokens_before, config.keep_recent_tokens, summarizer)
            .await
    }

    /// Force a compaction attempt even if the threshold has not yet
    /// been exceeded. Returns `Ok(None)` if there isn't enough
    /// discardable context to compact.
    pub async fn force_compact(
        &mut self,
        config: &CompactionConfig,
        summarizer: &dyn MessageSummarizer,
    ) -> Result<Option<CompactionResult>> {
        let tokens_before = self.estimate_context_tokens();
        self.compact_internal(tokens_before, config.keep_recent_tokens, summarizer)
            .await
    }

    /// Return the latest compaction summary on the active branch, if any.
    #[must_use]
    pub fn latest_compaction_summary(&self) -> Option<String> {
        let leaf_id = self.leaf_id()?;
        self.get_branch(leaf_id)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { summary, .. } => Some(summary.clone()),
                _ => None,
            })
    }

    /// Return the typed details payload from the most recent
    /// compaction on the active branch. `None` when the session
    /// has no compactions or when the most recent compaction
    /// had no tracked tool calls.
    ///
    /// Callers (e.g. resume-time UI, a future system-prompt
    /// hint generator) read this to show which files the agent
    /// touched during summarized intervals without re-parsing
    /// `<read-files>` / `<modified-files>` tag blocks out of
    /// the summary text.
    #[must_use]
    pub fn latest_compaction_details(&self) -> Option<CompactionDetails> {
        let leaf_id = self.leaf_id()?;
        self.get_branch(leaf_id)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { details, .. } => details.clone(),
                _ => None,
            })
    }

    /// List all session files in `sessions_dir`, newest first.
    pub fn list_sessions(sessions_dir: &Path) -> Result<Vec<SessionInfo>> {
        if !sessions_dir.exists() {
            return Ok(Vec::new());
        }

        let mut sessions = Vec::new();
        for entry in fs::read_dir(sessions_dir)
            .with_context(|| format!("failed to read {}", sessions_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }

            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut lines = content.lines();
            let Some(header_line) = lines.next() else {
                continue;
            };
            let Ok(header) = serde_json::from_str::<SessionHeader>(header_line) else {
                continue;
            };

            let mut message_count = 0u32;
            let mut first_message = String::new();
            for line in lines {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(entry) = serde_json::from_str::<SessionEntry>(trimmed) else {
                    continue;
                };
                if let SessionEntry::Message { message, .. } = entry {
                    message_count = message_count.saturating_add(1);
                    if first_message.is_empty()
                        && let Message::User(user) = message
                    {
                        first_message = join_text_content(&user.content);
                    }
                }
            }

            let metadata = fs::metadata(&path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            sessions.push(SessionInfo {
                path,
                id: header.id,
                cwd: header.cwd,
                created: header.timestamp,
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                message_count,
                first_message,
            });
        }

        sessions.sort_by(|left, right| right.modified.cmp(&left.modified));
        Ok(sessions)
    }

    fn generate_id(&self) -> String {
        generate_unique_id(|id| self.by_id.contains_key(id))
    }

    async fn compact_internal(
        &mut self,
        tokens_before: u64,
        keep_recent_tokens: u64,
        summarizer: &dyn MessageSummarizer,
    ) -> Result<Option<CompactionResult>> {
        let context = self.build_context();
        let Ok(cut_point) = find_cut_point(&context.messages, keep_recent_tokens) else {
            return Ok(None);
        };
        let first_kept_entry_id = cut_point.first_kept_entry_id.clone();

        let source_messages = cut_point
            .discarded
            .iter()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>();
        let existing_summary = self.latest_compaction_summary();

        // Split-turn branch: when the cut lands inside a turn,
        // run two summarizations in parallel — one over the
        // main history (everything before the split turn) and
        // one over the turn prefix. Joined per pi's format so
        // the resumed context keeps both bands of prose. Clean-
        // boundary cuts fall through to the single-summary path.
        let prose = if let Some(split) = &cut_point.split_turn {
            let (main_messages, prefix_messages) =
                partition_split_turn(&cut_point.discarded, split);
            let main_source: Vec<Message> =
                main_messages.iter().map(|m| m.message.clone()).collect();
            let prefix_source: Vec<Message> =
                prefix_messages.iter().map(|m| m.message.clone()).collect();
            // futures::try_join! short-circuits on the first
            // error; matches pi's Promise.all semantics so one
            // failed summarization aborts the compaction. We
            // use futures (not tokio) to keep anie-session
            // runtime-agnostic.
            let (main_prose, prefix_prose) = futures::try_join!(
                summarizer.summarize(&main_source, existing_summary.as_deref()),
                summarizer.summarize(&prefix_source, None),
            )?;
            join_split_turn_prose(&main_prose, &prefix_prose)
        } else {
            summarizer
                .summarize(&source_messages, existing_summary.as_deref())
                .await?
        };

        // Extract file-op details from the discarded interval
        // and merge into the final summary via
        // `<read-files>` / `<modified-files>` tag blocks so
        // resumed sessions retain grep-able path records. The
        // typed mirror on `SessionEntry::Compaction.details`
        // gives programmatic access without re-parsing tags.
        let details = extract_compaction_details(&source_messages);
        let summary = append_details_to_summary(&prose, &details);
        let persisted_details = if details.is_empty() {
            None
        } else {
            Some(details)
        };

        let entry = SessionEntry::Compaction {
            base: EntryBase {
                id: self.generate_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: now_iso8601()?,
            },
            summary: summary.clone(),
            tokens_before,
            first_kept_entry_id: first_kept_entry_id.clone(),
            details: persisted_details,
        };
        self.add_entries(vec![entry])?;

        Ok(Some(CompactionResult {
            summary,
            tokens_before,
            first_kept_entry_id,
            messages_discarded: cut_point.discarded.len(),
        }))
    }
}

/// Parse a session JSONL file into a header and entry list.
pub fn parse_session_file(content: &str) -> Result<(SessionHeader, Vec<SessionEntry>)> {
    let mut lines = content.lines();
    let header_line = lines.next().ok_or_else(|| anyhow!("empty session file"))?;
    let header: SessionHeader =
        serde_json::from_str(header_line).context("failed to parse session header")?;

    let mut entries = Vec::new();
    for (index, line) in lines.enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEntry>(trimmed) {
            Ok(entry) => entries.push(entry),
            Err(error) => warn!(line = index + 2, %error, "skipping malformed session line"),
        }
    }

    Ok((header, entries))
}

/// Estimate token usage for a single canonical message.
#[must_use]
pub fn estimate_tokens(message: &Message) -> u64 {
    match message {
        Message::User(user) => content_tokens(&user.content),
        Message::Assistant(assistant) => content_tokens(&assistant.content),
        Message::ToolResult(tool_result) => content_tokens(&tool_result.content),
        Message::Custom(_) => 100,
    }
}

/// Estimate token usage for a session context.
///
/// Uses a hybrid strategy modeled on pi's
/// `calculateContextTokens`
/// (`packages/coding-agent/src/core/compaction/compaction.ts`):
///
/// 1. Walk newest → oldest. Stop at the first
///    `Message::Assistant` whose `usage.total_tokens > 0`.
/// 2. Use that reading as the running total for every message up
///    to and including that assistant turn.
/// 3. Add a chars/4 heuristic estimate for every message *after*
///    that turn (i.e. the trailing user / tool-result messages
///    that came in after the last usage-reporting response).
///
/// Falls back to a pure heuristic walk when no usage data is
/// found anywhere in the context.
///
/// Matches pi exactly — we don't add a 2× cap or model-switch
/// reset guard. If provider-reported usage proves unreliable
/// enough to warrant guardrails, add them as an anie-specific
/// follow-up (documented in
/// `docs/pi_adoption_plan/03_token_estimation.md`).
#[must_use]
pub fn estimate_context_tokens(messages: &[SessionContextMessage]) -> u64 {
    // Find the newest assistant turn with usage data.
    let latest_usage = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, entry)| match &entry.message {
            Message::Assistant(assistant) => {
                let total = assistant.usage.total_tokens.unwrap_or_else(|| {
                    // Some providers report component totals
                    // without a totalTokens field; reconstruct.
                    let u = &assistant.usage;
                    u.input_tokens
                        .saturating_add(u.output_tokens)
                        .saturating_add(u.cache_read_tokens)
                        .saturating_add(u.cache_write_tokens)
                });
                if total > 0 {
                    Some((index, total))
                } else {
                    None
                }
            }
            _ => None,
        });

    match latest_usage {
        Some((index, seed)) => {
            // Everything up to and including `index` is captured
            // by `seed`. Add the heuristic for the rest.
            let trailing: u64 = messages
                .iter()
                .skip(index + 1)
                .map(|entry| estimate_tokens(&entry.message))
                .sum();
            seed.saturating_add(trailing)
        }
        None => messages
            .iter()
            .map(|entry| estimate_tokens(&entry.message))
            .sum(),
    }
}

/// Build the LLM prompt used for context compaction.
#[must_use]
pub fn build_compaction_prompt(messages: &[Message], existing_summary: Option<&str>) -> String {
    let mut prompt = String::new();

    if let Some(existing_summary) = existing_summary {
        prompt.push_str("Below is an existing conversation summary followed by new messages. Update the summary to incorporate the new information. Merge rather than replace — preserve important details from the existing summary.\n\n");
        prompt.push_str("## Existing Summary\n\n");
        prompt.push_str(existing_summary);
        prompt.push_str("\n\n## New Messages to Incorporate\n\n");
    } else {
        prompt.push_str("Summarize the following conversation for context continuity. The summary will be used to maintain context in a coding assistant session.\n\n");
        prompt.push_str("## Messages\n\n");
    }

    for message in messages {
        match message {
            Message::User(user) => {
                prompt.push_str("User: ");
                prompt.push_str(&join_text_content(&user.content));
                prompt.push_str("\n\n");
            }
            Message::Assistant(assistant) => {
                prompt.push_str("Assistant: ");
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text } => prompt.push_str(text),
                        ContentBlock::ToolCall(tool_call) => {
                            prompt.push_str(&format!("[Called tool: {}]", tool_call.name));
                        }
                        ContentBlock::Thinking { thinking, .. } => prompt.push_str(thinking),
                        ContentBlock::RedactedThinking { .. } => {
                            prompt.push_str("[redacted reasoning]");
                        }
                        ContentBlock::Image { .. } => prompt.push_str("[Image omitted]"),
                    }
                }
                prompt.push_str("\n\n");
            }
            Message::ToolResult(tool_result) => {
                prompt.push_str(&format!("Tool result ({}): ", tool_result.tool_name));
                let body = join_text_content(&tool_result.content);
                if body.len() > 500 {
                    prompt.push_str(&body[..500]);
                    prompt.push_str("...[truncated]");
                } else {
                    prompt.push_str(&body);
                }
                prompt.push_str("\n\n");
            }
            Message::Custom(custom) => {
                prompt.push_str(&format!(
                    "Custom message ({}): {}\n\n",
                    custom.custom_type, custom.content
                ));
            }
        }
    }

    prompt.push_str(
        "Provide a structured summary with these sections:\n\
1. **Goal**: What the user is trying to accomplish\n\
2. **Progress**: What has been done so far (completed tasks, key decisions)\n\
3. **Key Decisions**: Important choices made and their rationale\n\
4. **Files Modified**: List of files that were read or modified\n\
5. **Next Steps**: What remains to be done, if apparent\n\
6. **Critical Context**: Any constraints, preferences, or important details to preserve\n\n\
Keep the summary concise but comprehensive. Focus on information needed to continue the work.",
    );

    prompt
}

/// Result of compaction-cut analysis. Replaces the prior tuple
/// return so plan 06 PR C can carry `split_turn` metadata
/// without another breaking change.
#[derive(Debug, Clone, PartialEq)]
pub struct CutPoint {
    /// Messages that will be summarized / discarded.
    pub discarded: Vec<SessionContextMessage>,
    /// `entry_id` of the first message that would be kept.
    /// Callers that need the kept slice itself walk their own
    /// context by this entry id — the cut point no longer
    /// clones the kept vector just to hand it back. Plan 03 PR-D.
    pub first_kept_entry_id: String,
    /// When the cut lands mid-turn, the prefix of the turn that
    /// belongs on the discarded side. `None` when the cut
    /// happens cleanly at a turn boundary.
    pub split_turn: Option<SplitTurn>,
}

/// Information about a turn that straddles the compaction cut.
/// `prefix_entry_ids` is the set of `entry_id` values from
/// `CutPoint::discarded` that belong to the split turn; the
/// rest of the turn stays in `kept`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitTurn {
    /// Entry ID of the user message that started the turn
    /// straddling the cut.
    pub turn_start_entry_id: String,
    /// Discarded-side entry IDs for the turn prefix.
    pub prefix_entry_ids: Vec<String>,
}

/// Find the compaction cut point for the provided active-branch context.
pub fn find_cut_point(
    messages: &[SessionContextMessage],
    keep_recent_tokens: u64,
) -> Result<CutPoint> {
    let mut accumulated = 0u64;
    let mut cut_index = messages.len();

    for (index, message) in messages.iter().enumerate().rev() {
        accumulated = accumulated.saturating_add(estimate_tokens(&message.message));
        if accumulated >= keep_recent_tokens {
            cut_index = index.saturating_add(1);
            break;
        }
    }

    while cut_index < messages.len() {
        match &messages[cut_index].message {
            Message::ToolResult(_) => cut_index += 1,
            _ => break,
        }
    }

    if cut_index == 0 || cut_index >= messages.len() {
        return Err(anyhow!("cannot compact: not enough messages to discard"));
    }

    // Plan 03 PR-D: clone only `discarded` + the single
    // first-kept entry id. The previous shape cloned the
    // entire `kept` slice too, which compact_internal never
    // read.
    let discarded = messages[..cut_index].to_vec();
    let first_kept_entry_id = messages[cut_index].entry_id.clone();
    let split_turn = detect_split_turn(messages, cut_index);
    Ok(CutPoint {
        discarded,
        first_kept_entry_id,
        split_turn,
    })
}

/// Split the discarded slice into (main, prefix) relative to a
/// `SplitTurn`. Prefix entries keep their original order; main
/// is everything in `discarded` that isn't in the prefix.
fn partition_split_turn<'a>(
    discarded: &'a [SessionContextMessage],
    split: &SplitTurn,
) -> (
    Vec<&'a SessionContextMessage>,
    Vec<&'a SessionContextMessage>,
) {
    let prefix_ids: std::collections::HashSet<&str> =
        split.prefix_entry_ids.iter().map(String::as_str).collect();
    let mut main: Vec<&SessionContextMessage> = Vec::new();
    let mut prefix: Vec<&SessionContextMessage> = Vec::new();
    for entry in discarded {
        if prefix_ids.contains(entry.entry_id.as_str()) {
            prefix.push(entry);
        } else {
            main.push(entry);
        }
    }
    (main, prefix)
}

/// Join two prose summaries per pi's split-turn format: main
/// history first, then a `---` separator, then the prefix under
/// a labeled heading. See
/// `packages/coding-agent/src/core/compaction/compaction.ts:715`
/// for pi's exact string.
#[must_use]
pub fn join_split_turn_prose(main_prose: &str, prefix_prose: &str) -> String {
    format!(
        "{}\n\n---\n\n**Turn Context (split turn):**\n\n{}",
        main_prose.trim_end(),
        prefix_prose.trim()
    )
}

/// Detect whether `cut_index` lands inside a turn (a User
/// message followed by Assistant responses + tool results).
/// Returns `Some(SplitTurn)` when the kept side starts with a
/// non-User message, `None` on clean turn-boundary cuts.
fn detect_split_turn(messages: &[SessionContextMessage], cut_index: usize) -> Option<SplitTurn> {
    if cut_index == 0 || cut_index >= messages.len() {
        return None;
    }
    // Clean cut: the kept side starts with a User message.
    if matches!(messages[cut_index].message, Message::User(_)) {
        return None;
    }
    // Walk back to the User that started the straddling turn.
    let turn_start_idx = (0..cut_index)
        .rev()
        .find(|idx| matches!(messages[*idx].message, Message::User(_)))?;
    let prefix_entry_ids = messages[turn_start_idx..cut_index]
        .iter()
        .map(|m| m.entry_id.clone())
        .collect();
    Some(SplitTurn {
        turn_start_entry_id: messages[turn_start_idx].entry_id.clone(),
        prefix_entry_ids,
    })
}

/// Plan 03 PR-A: closure-based existence predicate so this
/// helper decouples from any particular membership container.
/// Callers pass a closure backed by `HashSet::contains` (for
/// the filesystem-scan path in `new_session_with_parent`) or
/// by `HashMap::contains_key` (for the in-memory path in
/// `generate_id`).
fn generate_unique_id(exists: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id = Uuid::new_v4().simple().to_string()[..8].to_string();
        if !exists(&id) {
            return id;
        }
    }
    Uuid::new_v4().to_string()
}

fn content_tokens(blocks: &[ContentBlock]) -> u64 {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => (text.len() as u64) / 4,
            ContentBlock::Image { .. } => 1_200,
            ContentBlock::Thinking { thinking, .. } => (thinking.len() as u64) / 4,
            ContentBlock::RedactedThinking { data } => (data.len() as u64) / 4,
            ContentBlock::ToolCall(tool_call) => {
                let args_len = serde_json::to_string(&tool_call.arguments)
                    .map(|value| value.len())
                    .unwrap_or_default();
                (tool_call.name.len() as u64 + args_len as u64) / 4
            }
        })
        .sum()
}

fn join_text_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn now_iso8601() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("failed to format timestamp")
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tempfile::tempdir;

    use super::*;
    use anie_protocol::{AssistantMessage, StopReason, ToolResultMessage, Usage};

    /// Test double for `MessageSummarizer`. Records every call and
    /// returns a pre-baked summary string.
    struct RecordingSummarizer {
        summary: String,
        calls: std::sync::Mutex<Vec<(Vec<Message>, Option<String>)>>,
    }

    impl RecordingSummarizer {
        fn with_summary(summary: &str) -> Self {
            Self {
                summary: summary.to_string(),
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MessageSummarizer for RecordingSummarizer {
        async fn summarize(
            &self,
            messages: &[Message],
            existing_summary: Option<&str>,
        ) -> Result<String> {
            self.calls
                .lock()
                .expect("summarizer call log lock")
                .push((messages.to_vec(), existing_summary.map(str::to_string)));
            Ok(self.summary.clone())
        }
    }

    fn user_message(text: &str, timestamp: u64) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            timestamp,
        })
    }

    fn assistant_message(text: &str, timestamp: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp,
            reasoning_details: None,
        })
    }

    fn assistant_message_with_thinking(thinking: &str, text: &str, timestamp: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: thinking.to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: text.to_string(),
                },
            ],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp,
            reasoning_details: None,
        })
    }

    #[test]
    fn session_roundtrip_survives_reopen() {
        let tempdir = tempdir().expect("tempdir");
        let sessions_dir = tempdir.path().join("sessions");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");

        let mut session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
        let user_id = session
            .append_message(&user_message("hello", 1))
            .expect("append user");
        session
            .append_messages(&[assistant_message("hi", 2)])
            .expect("append assistant");
        drop(session);

        let session_path = sessions_dir
            .read_dir()
            .expect("read dir")
            .next()
            .expect("session file")
            .expect("dir entry")
            .path();
        let reopened = SessionManager::open_session(&session_path).expect("open session");

        assert_eq!(reopened.entries.len(), 2);
        assert_eq!(
            reopened.leaf_id(),
            Some(reopened.entries[1].base().id.as_str())
        );
        assert!(reopened.get_entry(&user_id).is_some());
    }

    #[test]
    fn session_roundtrip_preserves_thinking_blocks_after_reopen() {
        let tempdir = tempdir().expect("tempdir");
        let sessions_dir = tempdir.path().join("sessions");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");

        let mut session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
        session
            .append_message(&user_message("hello", 1))
            .expect("append user");
        session
            .append_message(&assistant_message_with_thinking("plan first", "done", 2))
            .expect("append assistant");
        drop(session);

        let session_path = sessions_dir
            .read_dir()
            .expect("read dir")
            .next()
            .expect("session file")
            .expect("dir entry")
            .path();
        let reopened = SessionManager::open_session(&session_path).expect("open session");
        let context = reopened.build_context();

        assert!(matches!(
            &context.messages[1].message,
            Message::Assistant(AssistantMessage { content, .. })
                if content.iter().any(|block| matches!(block, ContentBlock::Thinking { thinking, .. } if thinking == "plan first"))
                    && content.iter().any(|block| matches!(block, ContentBlock::Text { text } if text == "done"))
        ));
    }

    #[test]
    fn session_roundtrip_preserves_reasoning_details_after_reopen() {
        let tempdir = tempdir().expect("tempdir");
        let sessions_dir = tempdir.path().join("sessions");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");

        let assistant = Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "openrouter".into(),
            model: "openai/o3".into(),
            timestamp: 2,
            reasoning_details: Some(vec![serde_json::json!({
                "type": "reasoning.encrypted",
                "id": "call_abc",
                "data": "OPAQUE_PAYLOAD",
            })]),
        });

        let mut session = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
        session
            .append_message(&user_message("hello", 1))
            .expect("append user");
        session
            .append_message(&assistant)
            .expect("append assistant");
        drop(session);

        let session_path = sessions_dir
            .read_dir()
            .expect("read dir")
            .next()
            .expect("session file")
            .expect("dir entry")
            .path();
        let reopened = SessionManager::open_session(&session_path).expect("open session");
        let context = reopened.build_context();

        let Message::Assistant(replayed) = &context.messages[1].message else {
            panic!("expected assistant message");
        };
        let details = replayed
            .reasoning_details
            .as_ref()
            .expect("reasoning_details survived reopen");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["data"], "OPAQUE_PAYLOAD");
    }

    #[test]
    fn session_reopen_tolerates_pre_v3_files_without_reasoning_details() {
        // A schema v2 session line (no `reasoning_details` key) must
        // still deserialize — the field defaults to `None`.
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("legacy-v2.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 2,
            "id": "legacy-v2",
            "timestamp": "2026-04-14T00:00:00Z",
            "cwd": "/tmp",
        });
        let entry = serde_json::json!({
            "type": "message",
            "id": "11111111",
            "parentId": null,
            "timestamp": "2026-04-14T00:00:01Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_write_tokens": 0
                },
                "stop_reason": "Stop",
                "provider": "openrouter",
                "model": "openai/o3",
                "timestamp": 1
            }
        });
        fs::write(&path, format!("{header}\n{entry}\n")).expect("write");

        let session = SessionManager::open_session(&path).expect("open v2 session");
        let SessionEntry::Message { message, .. } = &session.entries[0] else {
            panic!("expected message");
        };
        let Message::Assistant(assistant) = message else {
            panic!("expected assistant");
        };
        assert!(assistant.reasoning_details.is_none());
    }

    #[test]
    fn build_context_replaces_old_messages_after_compaction() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");

        let first = session
            .append_message(&user_message("first", 1))
            .expect("first");
        let second = session
            .append_message(&assistant_message("second", 2))
            .expect("second");
        let third = session
            .append_message(&user_message("third", 3))
            .expect("third");
        let fourth = session
            .append_message(&assistant_message("fourth", 4))
            .expect("fourth");
        session
            .append_model_change("anthropic", "claude-sonnet-4-6")
            .expect("model change");
        session
            .append_thinking_change(ThinkingLevel::High)
            .expect("thinking change");
        session
            .add_entries(vec![SessionEntry::Compaction {
                base: EntryBase {
                    id: session.generate_id(),
                    parent_id: session.leaf_id().map(str::to_string),
                    timestamp: now_iso8601().expect("timestamp"),
                },
                summary: "summary text".into(),
                tokens_before: 200,
                first_kept_entry_id: third.clone(),
                details: None,
            }])
            .expect("append compaction");

        let context = session.build_context();
        assert_eq!(
            context.model,
            Some(("anthropic".into(), "claude-sonnet-4-6".into()))
        );
        assert_eq!(context.thinking_level, Some(ThinkingLevel::High));
        assert_eq!(context.messages.len(), 3);
        assert!(
            matches!(&context.messages[0].message, Message::User(user) if join_text_content(&user.content).contains("summary text"))
        );
        assert_eq!(context.messages[1].entry_id, third);
        assert_eq!(context.messages[2].entry_id, fourth);
        assert_ne!(context.messages[1].entry_id, first);
        assert_ne!(context.messages[1].entry_id, second);
    }

    #[test]
    fn estimate_context_tokens_matches_build_context_totals() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");

        assert_eq!(session.estimate_context_tokens(), 0);
        assert_eq!(
            session.estimate_context_tokens(),
            estimate_context_tokens(&session.build_context().messages)
        );

        session
            .append_message(&user_message(&"a".repeat(400), 1))
            .expect("append user");
        session
            .append_message(&assistant_message(&"b".repeat(400), 2))
            .expect("append assistant");
        let third = session
            .append_message(&user_message(&"c".repeat(400), 3))
            .expect("append third");
        session
            .append_message(&assistant_message(&"d".repeat(400), 4))
            .expect("append fourth");

        let full_total = estimate_context_tokens(&session.build_context().messages);
        assert_eq!(session.estimate_context_tokens(), full_total);
        assert!(full_total > 0);

        session
            .add_entries(vec![SessionEntry::Compaction {
                base: EntryBase {
                    id: session.generate_id(),
                    parent_id: session.leaf_id().map(str::to_string),
                    timestamp: now_iso8601().expect("timestamp"),
                },
                summary: "rolled-up summary".into(),
                tokens_before: full_total,
                first_kept_entry_id: third,
                details: None,
            }])
            .expect("append compaction");

        assert_eq!(
            session.estimate_context_tokens(),
            estimate_context_tokens(&session.build_context().messages)
        );
    }

    fn assistant_message_with_usage(text: &str, timestamp: u64, total_tokens: u64) -> Message {
        let usage = Usage {
            input_tokens: total_tokens / 2,
            output_tokens: total_tokens - total_tokens / 2,
            total_tokens: Some(total_tokens),
            ..Usage::default()
        };
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            usage,
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp,
            reasoning_details: None,
        })
    }

    fn context_from(messages: Vec<Message>) -> Vec<SessionContextMessage> {
        messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| SessionContextMessage {
                entry_id: format!("entry-{index}"),
                message,
            })
            .collect()
    }

    #[test]
    fn estimate_context_tokens_falls_back_to_heuristic_without_usage() {
        let context = context_from(vec![
            user_message("hello", 1),
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "hi there".into(),
                }],
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                provider: "mock".into(),
                model: "mock-model".into(),
                timestamp: 2,
                reasoning_details: None,
            }),
        ]);
        // "hello" = 5 chars → 1 token via chars/4; "hi there"
        // = 8 chars → 2 tokens. Both heuristic.
        let got = estimate_context_tokens(&context);
        let expected_heuristic: u64 = context.iter().map(|m| estimate_tokens(&m.message)).sum();
        assert_eq!(got, expected_heuristic);
        assert!(got > 0);
    }

    #[test]
    fn estimate_context_tokens_seeds_from_latest_assistant_usage() {
        // One assistant turn with usage reports 5_000 total;
        // there's no trailing message, so the estimate should
        // be exactly 5_000 — independent of the heuristic for
        // the prior user turn.
        let context = context_from(vec![
            user_message("a".repeat(10_000).as_str(), 1),
            assistant_message_with_usage("response", 2, 5_000),
        ]);
        assert_eq!(estimate_context_tokens(&context), 5_000);
    }

    #[test]
    fn estimate_context_tokens_adds_trailing_heuristic_after_usage() {
        // Usage = 5_000 reported at step 2; then a user turn
        // and a tool result come after. Those are approximated
        // via chars/4 and added on top.
        let context = context_from(vec![
            user_message("first", 1),
            assistant_message_with_usage("response", 2, 5_000),
            user_message(&"c".repeat(400), 3),
        ]);
        let trailing = estimate_tokens(&context[2].message);
        assert_eq!(estimate_context_tokens(&context), 5_000 + trailing);
    }

    #[test]
    fn estimate_context_tokens_prefers_newest_usage_over_older() {
        // Two assistant turns; usage numbers differ. The newer
        // reading wins.
        let context = context_from(vec![
            user_message("q1", 1),
            assistant_message_with_usage("a1", 2, 1_000),
            user_message("q2", 3),
            assistant_message_with_usage("a2", 4, 4_000),
        ]);
        assert_eq!(estimate_context_tokens(&context), 4_000);
    }

    #[test]
    fn estimate_context_tokens_ignores_assistant_with_zero_usage() {
        // An assistant turn with `total_tokens: 0` (possibly an
        // early error case) shouldn't override the earlier usage.
        let context = context_from(vec![
            user_message("q1", 1),
            assistant_message_with_usage("a1", 2, 3_000),
            user_message("q2", 3),
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "error".into(),
                }],
                usage: Usage::default(),
                stop_reason: StopReason::Error,
                error_message: Some("err".into()),
                provider: "mock".into(),
                model: "mock-model".into(),
                timestamp: 4,
                reasoning_details: None,
            }),
        ]);
        let trailing = estimate_tokens(&context[2].message) + estimate_tokens(&context[3].message);
        assert_eq!(estimate_context_tokens(&context), 3_000 + trailing);
    }

    #[test]
    fn estimate_context_tokens_reconstructs_total_from_components_when_missing() {
        // Some providers populate input/output but not
        // total_tokens. Reconstruct.
        let usage = Usage {
            input_tokens: 2_000,
            output_tokens: 800,
            cache_read_tokens: 100,
            cache_write_tokens: 200,
            ..Usage::default()
        };
        let context = context_from(vec![
            user_message("q", 1),
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text { text: "a".into() }],
                usage,
                stop_reason: StopReason::Stop,
                error_message: None,
                provider: "mock".into(),
                model: "mock-model".into(),
                timestamp: 2,
                reasoning_details: None,
            }),
        ]);
        assert_eq!(estimate_context_tokens(&context), 3_100);
    }

    #[test]
    fn fork_to_child_session_clones_active_branch() {
        let tempdir = tempdir().expect("tempdir");
        let sessions_dir = tempdir.path().join("sessions");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");

        let mut parent = SessionManager::new_session(&sessions_dir, &cwd).expect("new session");
        parent
            .append_message(&user_message("hello", 1))
            .expect("append user");
        parent
            .append_message(&assistant_message("world", 2))
            .expect("append assistant");
        parent
            .append_model_change("anthropic", "claude-sonnet-4-6")
            .expect("model change");
        parent
            .append_thinking_change(ThinkingLevel::High)
            .expect("thinking change");

        let child = parent
            .fork_to_child_session(&sessions_dir)
            .expect("fork child session");
        assert_ne!(parent.id(), child.id());
        assert_eq!(child.header().parent_session.as_deref(), Some(parent.id()));
        assert_eq!(parent.build_context(), child.build_context());
    }

    #[test]
    fn list_sessions_returns_newest_first() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");

        let mut first = SessionManager::new_session(tempdir.path(), &cwd).expect("first session");
        first
            .append_message(&user_message("alpha", 1))
            .expect("append first");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second = SessionManager::new_session(tempdir.path(), &cwd).expect("second session");
        second
            .append_message(&user_message("beta", 2))
            .expect("append second");

        let sessions = SessionManager::list_sessions(tempdir.path()).expect("list sessions");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].first_message, "beta");
        assert_eq!(sessions[1].first_message, "alpha");
    }

    #[test]
    fn new_session_writes_current_schema_version() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");
        assert_eq!(session.header.version, CURRENT_SESSION_SCHEMA_VERSION);
    }

    #[test]
    fn open_session_accepts_older_schema_versions() {
        // Simulate a session file written by a pre-fix binary:
        // schema_version = 1, thinking block with no `signature` key.
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("legacy.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 1,
            "id": "legacy01",
            "timestamp": "2026-04-14T00:00:00Z",
            "cwd": "/tmp",
        });
        let entry = serde_json::json!({
            "type": "message",
            "id": "11111111",
            "parentId": null,
            "timestamp": "2026-04-14T00:00:01Z",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "old reasoning"},
                    {"type": "text", "text": "answer"}
                ],
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_write_tokens": 0,
                    "cost": {
                        "input": 0.0,
                        "output": 0.0,
                        "cache_read": 0.0,
                        "cache_write": 0.0,
                        "total": 0.0
                    }
                },
                "stop_reason": "Stop",
                "provider": "anthropic",
                "model": "claude-sonnet-4-6",
                "timestamp": 1,
            }
        });
        fs::write(&path, format!("{header}\n{entry}\n")).expect("write");

        let session = SessionManager::open_session(&path).expect("load v1 session");
        assert_eq!(session.header.version, 1);
        assert_eq!(session.entries.len(), 1);
        // Verify the legacy thinking block round-trips as
        // Thinking { signature: None }.
        let SessionEntry::Message { message, .. } = &session.entries[0] else {
            panic!("expected message");
        };
        let Message::Assistant(a) = message else {
            panic!("expected assistant");
        };
        assert!(a.content.iter().any(|b| matches!(
            b,
            ContentBlock::Thinking { signature: None, thinking } if thinking == "old reasoning"
        )));
    }

    fn assistant_with_tool_calls(calls: Vec<(&str, serde_json::Value)>, timestamp: u64) -> Message {
        assistant_with_tool_calls_and_text("", calls, timestamp)
    }

    fn assistant_with_tool_calls_and_text(
        text: &str,
        calls: Vec<(&str, serde_json::Value)>,
        timestamp: u64,
    ) -> Message {
        use anie_protocol::ToolCall;
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
        for (idx, (name, arguments)) in calls.into_iter().enumerate() {
            blocks.push(ContentBlock::ToolCall(ToolCall {
                id: format!("call-{idx}"),
                name: name.to_string(),
                arguments,
            }));
        }
        Message::Assistant(AssistantMessage {
            content: blocks,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp,
            reasoning_details: None,
        })
    }

    #[test]
    fn extract_compaction_details_picks_up_read_and_write_paths() {
        let messages = vec![
            user_message("prompt", 1),
            assistant_with_tool_calls(
                vec![
                    ("read", serde_json::json!({"path": "src/a.rs"})),
                    ("write", serde_json::json!({"path": "src/b.rs"})),
                ],
                2,
            ),
            assistant_with_tool_calls(vec![("edit", serde_json::json!({"path": "src/c.rs"}))], 3),
        ];
        let details = extract_compaction_details(&messages);
        assert_eq!(details.read_files, vec!["src/a.rs"]);
        assert_eq!(details.modified_files, vec!["src/b.rs", "src/c.rs"]);
    }

    #[test]
    fn extract_compaction_details_dedupes_repeated_paths() {
        let messages = vec![
            assistant_with_tool_calls(
                vec![
                    ("read", serde_json::json!({"path": "src/a.rs"})),
                    ("read", serde_json::json!({"path": "src/a.rs"})),
                    ("read", serde_json::json!({"path": "src/b.rs"})),
                ],
                1,
            ),
            assistant_with_tool_calls(vec![("edit", serde_json::json!({"path": "src/a.rs"}))], 2),
            assistant_with_tool_calls(vec![("edit", serde_json::json!({"path": "src/a.rs"}))], 3),
        ];
        let details = extract_compaction_details(&messages);
        assert_eq!(details.read_files, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(details.modified_files, vec!["src/a.rs"]);
    }

    #[test]
    fn extract_compaction_details_ignores_unknown_tool_names() {
        // Custom tools outside the read/write/edit triad should
        // never populate details even if they carry a "path" arg.
        let messages = vec![assistant_with_tool_calls(
            vec![
                ("bash", serde_json::json!({"command": "ls"})),
                ("my-custom-reader", serde_json::json!({"path": "src/a.rs"})),
            ],
            1,
        )];
        let details = extract_compaction_details(&messages);
        assert!(details.is_empty(), "{details:?}");
    }

    #[test]
    fn extract_compaction_details_accepts_file_path_alias() {
        // Some provider tool-call conventions name the arg
        // `file_path` instead of `path`; we support both so the
        // list stays comprehensive across third-party tools.
        let messages = vec![assistant_with_tool_calls(
            vec![("read", serde_json::json!({"file_path": "src/x.rs"}))],
            1,
        )];
        let details = extract_compaction_details(&messages);
        assert_eq!(details.read_files, vec!["src/x.rs"]);
    }

    #[test]
    fn extract_compaction_details_skips_calls_without_path() {
        let messages = vec![assistant_with_tool_calls(
            vec![("read", serde_json::json!({"offset": 10}))],
            1,
        )];
        let details = extract_compaction_details(&messages);
        assert!(details.is_empty());
    }

    #[test]
    fn append_details_to_summary_injects_xml_tags() {
        let details = CompactionDetails {
            read_files: vec!["src/a.rs".into(), "src/b.rs".into()],
            modified_files: vec!["src/a.rs".into()],
        };
        let out = append_details_to_summary("prose body", &details);
        assert!(
            out.contains("<read-files>\nsrc/a.rs\nsrc/b.rs\n</read-files>"),
            "{out}"
        );
        assert!(
            out.contains("<modified-files>\nsrc/a.rs\n</modified-files>"),
            "{out}"
        );
        assert!(out.starts_with("prose body"), "{out}");
    }

    #[test]
    fn append_details_to_summary_is_identity_when_details_empty() {
        let details = CompactionDetails::default();
        let out = append_details_to_summary("prose body", &details);
        assert_eq!(out, "prose body");
    }

    #[test]
    fn append_details_to_summary_omits_empty_section() {
        let details = CompactionDetails {
            read_files: vec!["src/a.rs".into()],
            modified_files: Vec::new(),
        };
        let out = append_details_to_summary("prose", &details);
        assert!(out.contains("<read-files>"), "{out}");
        assert!(
            !out.contains("<modified-files>"),
            "empty modified section should not appear: {out}"
        );
    }

    #[test]
    fn compaction_entry_roundtrips_with_details_payload() {
        // Regression: v4 compaction entries carrying file-op
        // details must serialize + deserialize without loss.
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("proj");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");

        let first = session
            .append_message(&user_message("a", 1))
            .expect("append user");
        session
            .append_message(&assistant_message("b", 2))
            .expect("append assistant");
        let second_user = session
            .append_message(&user_message("c", 3))
            .expect("append third");

        session
            .add_entries(vec![SessionEntry::Compaction {
                base: EntryBase {
                    id: session.generate_id(),
                    parent_id: session.leaf_id().map(str::to_string),
                    timestamp: now_iso8601().expect("ts"),
                },
                summary: "rolled up".into(),
                tokens_before: 400,
                first_kept_entry_id: second_user,
                details: Some(CompactionDetails {
                    read_files: vec!["src/a.rs".into(), "src/b.rs".into()],
                    modified_files: vec!["src/a.rs".into()],
                }),
            }])
            .expect("append compaction");
        let session_path = session.path().to_path_buf();
        drop(session);

        let reopened = SessionManager::open_session(&session_path).expect("reopen");
        let compaction = reopened
            .entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { details, .. } => Some(details.clone()),
                _ => None,
            })
            .expect("compaction entry");
        let details = compaction.expect("details present");
        assert_eq!(details.read_files, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(details.modified_files, vec!["src/a.rs"]);
        let _ = first;
    }

    #[test]
    fn compaction_entry_without_details_omits_field_on_disk() {
        // Skip-serializing-if: a None `details` must not appear
        // at all in the JSONL representation, keeping v3-era
        // readers happy for entries that don't need the field.
        let entry = SessionEntry::Compaction {
            base: EntryBase {
                id: "11111111".into(),
                parent_id: None,
                timestamp: "2026-04-21T00:00:00Z".into(),
            },
            summary: "s".into(),
            tokens_before: 1,
            first_kept_entry_id: "22222222".into(),
            details: None,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(
            !json.contains("\"details\""),
            "None details leaked into JSON: {json}"
        );
    }

    #[test]
    fn pre_v4_compaction_entry_loads_with_details_defaulting_to_none() {
        // Forward-compat: a v3-era compaction entry (no details
        // field) must deserialize cleanly with details = None.
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("v3_compact.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": "v3compact",
            "timestamp": "2026-04-21T00:00:00Z",
            "cwd": "/tmp",
        });
        let entry = serde_json::json!({
            "type": "compaction",
            "id": "11111111",
            "parentId": null,
            "timestamp": "2026-04-21T00:00:01Z",
            "summary": "older summary",
            "tokens_before": 120,
            "firstKeptEntryId": "22222222"
        });
        fs::write(&path, format!("{header}\n{entry}\n")).expect("write");

        let session = SessionManager::open_session(&path).expect("load v3 session");
        let details = session
            .entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { details, .. } => Some(details.clone()),
                _ => None,
            })
            .expect("compaction entry");
        assert!(details.is_none(), "details must default to None on v3");
    }

    #[test]
    fn empty_compaction_details_vectors_drop_from_serialization() {
        // skip_serializing_if keeps the persisted payload small
        // when only one of read_files / modified_files is set.
        let details = CompactionDetails {
            read_files: vec!["src/a.rs".into()],
            modified_files: Vec::new(),
        };
        let json = serde_json::to_string(&details).expect("serialize");
        assert!(json.contains("read_files"), "{json}");
        assert!(
            !json.contains("modified_files"),
            "empty modified_files leaked: {json}"
        );
    }

    #[test]
    fn open_session_rejects_future_schema_versions() {
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().join("future.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": CURRENT_SESSION_SCHEMA_VERSION + 5,
            "id": "future01",
            "timestamp": "2026-04-14T00:00:00Z",
            "cwd": "/tmp",
        });
        fs::write(&path, format!("{header}\n")).expect("write");

        let Err(err) = SessionManager::open_session(&path) else {
            panic!("must reject future schema");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("schema version"), "error was: {msg}");
        assert!(msg.contains("upgrade anie"), "error was: {msg}");
    }

    #[test]
    fn parser_skips_malformed_lines() {
        let content = format!(
            "{}\n{}\nnot-json\n{}\n",
            serde_json::to_string(&SessionHeader {
                entry_type: "session".into(),
                version: 1,
                id: "abcd1234".into(),
                timestamp: "2026-04-14T00:00:00Z".into(),
                cwd: "/tmp".into(),
                parent_session: None,
            })
            .expect("header"),
            serde_json::to_string(&SessionEntry::Message {
                base: EntryBase {
                    id: "11111111".into(),
                    parent_id: None,
                    timestamp: "2026-04-14T00:00:01Z".into(),
                },
                message: user_message("hello", 1),
            })
            .expect("entry"),
            serde_json::to_string(&SessionEntry::Message {
                base: EntryBase {
                    id: "22222222".into(),
                    parent_id: Some("11111111".into()),
                    timestamp: "2026-04-14T00:00:02Z".into(),
                },
                message: assistant_message("world", 2),
            })
            .expect("entry"),
        );

        let (_header, entries) = parse_session_file(&content).expect("parse session");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn cut_point_skips_tool_results_at_boundary() {
        let messages = vec![
            SessionContextMessage {
                entry_id: "1".into(),
                message: user_message("old prompt", 1),
            },
            SessionContextMessage {
                entry_id: "2".into(),
                message: assistant_message("assistant", 2),
            },
            SessionContextMessage {
                entry_id: "3".into(),
                message: Message::ToolResult(ToolResultMessage {
                    tool_call_id: "call".into(),
                    tool_name: "read".into(),
                    content: vec![ContentBlock::Text {
                        text: "body".into(),
                    }],
                    details: serde_json::json!({"path": "file.txt"}),
                    is_error: false,
                    timestamp: 3,
                }),
            },
            SessionContextMessage {
                entry_id: "4".into(),
                message: user_message("new prompt", 4),
            },
        ];

        let cut_point = find_cut_point(&messages, 3).expect("cut point");
        assert_eq!(cut_point.discarded.len(), 3);
        // Plan 03 PR-D removed the `kept` field; the kept
        // slice is `messages[cut_index..]` derivable from
        // the entry_id. first_kept_entry_id still pins it.
        assert_eq!(cut_point.first_kept_entry_id, "4");
        assert!(
            cut_point.split_turn.is_none(),
            "PR C.1 preserves pre-split-turn behavior: {:?}",
            cut_point.split_turn
        );
    }

    #[test]
    fn find_cut_point_surfaces_split_turn_when_cut_lands_in_turn() {
        // Turn layout:
        //   [0] user  — old prompt
        //   [1] asst  — old response (full turn)
        //   [2] user  — active turn start
        //   [3] asst  — tool-calling assistant (big, forces cut to land here)
        //   [4] toolresult
        //   [5] asst  — completion (kept)
        let messages = vec![
            SessionContextMessage {
                entry_id: "1".into(),
                message: user_message(&"a".repeat(400), 1),
            },
            SessionContextMessage {
                entry_id: "2".into(),
                message: assistant_message(&"b".repeat(400), 2),
            },
            SessionContextMessage {
                entry_id: "3".into(),
                message: user_message(&"c".repeat(400), 3),
            },
            SessionContextMessage {
                entry_id: "4".into(),
                message: assistant_message(&"d".repeat(400), 4),
            },
            SessionContextMessage {
                entry_id: "5".into(),
                message: assistant_message(&"e".repeat(400), 5),
            },
        ];
        // 100 tokens per 400-char message; keep = 150 tokens
        // forces cut past index 4 and into the middle of the
        // turn started at "3".
        let cut = find_cut_point(&messages, 150).expect("cut");
        let split = cut
            .split_turn
            .expect("split turn should fire when cut lands mid-turn");
        assert_eq!(split.turn_start_entry_id, "3");
        // Prefix ids should include the user that started the
        // turn plus whatever discarded-side messages belong to
        // it (up to but not including the first kept).
        assert_eq!(split.prefix_entry_ids, vec!["3", "4"]);
    }

    #[test]
    fn find_cut_point_leaves_split_turn_none_on_clean_boundary() {
        // 4 messages, 100 tokens each. With keep=250 the loop
        // walks back through a2, u2, a1 — breaking at a1 sets
        // cut_index=2 so the kept slice begins on u2 (a User
        // message = clean boundary).
        let messages = vec![
            SessionContextMessage {
                entry_id: "1".into(),
                message: user_message(&"a".repeat(400), 1),
            },
            SessionContextMessage {
                entry_id: "2".into(),
                message: assistant_message(&"b".repeat(400), 2),
            },
            SessionContextMessage {
                entry_id: "3".into(),
                message: user_message(&"c".repeat(400), 3),
            },
            SessionContextMessage {
                entry_id: "4".into(),
                message: assistant_message(&"d".repeat(400), 4),
            },
        ];
        let cut = find_cut_point(&messages, 250).expect("cut");
        assert_eq!(cut.first_kept_entry_id, "3");
        assert!(cut.split_turn.is_none(), "{:?}", cut.split_turn);
    }

    #[test]
    fn join_split_turn_prose_uses_pi_format() {
        let out = join_split_turn_prose("main body  ", "  prefix body\n");
        // Verbatim check of the separator + heading line so
        // any drift from pi's shape surfaces on test diff.
        assert_eq!(
            out,
            "main body\n\n---\n\n**Turn Context (split turn):**\n\nprefix body"
        );
    }

    #[test]
    fn cut_point_struct_carries_all_fields() {
        // Regression for the tuple→struct refactor. Ensures all
        // four fields are observable from a single call.
        // Token budget and message lengths are chosen so the
        // cut lands partway through — not at either end.
        let messages = vec![
            SessionContextMessage {
                entry_id: "1".into(),
                message: user_message(&"a".repeat(800), 1),
            },
            SessionContextMessage {
                entry_id: "2".into(),
                message: assistant_message(&"b".repeat(800), 2),
            },
            SessionContextMessage {
                entry_id: "3".into(),
                message: user_message(&"c".repeat(800), 3),
            },
        ];
        let cut_point = find_cut_point(&messages, 300).expect("cut point");
        assert!(!cut_point.discarded.is_empty());
        // Plan 03 PR-D: kept slice derivable from
        // first_kept_entry_id + the caller's messages vec.
        // We assert the first kept entry id points at a real
        // message in the input.
        assert!(
            messages
                .iter()
                .any(|m| m.entry_id == cut_point.first_kept_entry_id),
            "first_kept_entry_id must match a message in the input"
        );
        assert!(cut_point.split_turn.is_none());
    }

    #[tokio::test]
    async fn auto_compact_invokes_summarizer_and_persists_entry() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");
        session
            .append_message(&user_message(&"a".repeat(3_000), 1))
            .expect("append first");
        session
            .append_message(&assistant_message(&"b".repeat(3_000), 2))
            .expect("append second");
        session
            .append_message(&user_message("recent prompt", 3))
            .expect("append third");

        let summarizer = RecordingSummarizer::with_summary("Goal\n\nProgress");

        let result = session
            .auto_compact(
                &CompactionConfig {
                    context_window: 1_000,
                    reserve_tokens: 100,
                    keep_recent_tokens: 100,
                },
                &summarizer,
            )
            .await
            .expect("auto compact")
            .expect("compaction result");

        assert!(result.summary.contains("Goal"));
        assert!(matches!(
            session.entries.last(),
            Some(SessionEntry::Compaction { .. })
        ));
        let context = session.build_context();
        assert!(
            matches!(&context.messages[0].message, Message::User(user) if join_text_content(&user.content).contains("Goal"))
        );
    }

    #[tokio::test]
    async fn auto_compact_populates_details_from_discarded_tool_calls() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");

        // Big enough to force a compaction cut past these entries.
        session
            .append_message(&user_message(&"a".repeat(3_000), 1))
            .expect("append first");
        session
            .append_message(&assistant_with_tool_calls_and_text(
                &"x".repeat(3_000),
                vec![
                    ("read", serde_json::json!({"path": "src/a.rs"})),
                    ("write", serde_json::json!({"path": "src/b.rs"})),
                ],
                2,
            ))
            .expect("append assistant with tools");
        session
            .append_message(&user_message("recent prompt", 3))
            .expect("append recent");

        let summarizer = RecordingSummarizer::with_summary("Prose summary.");

        session
            .auto_compact(
                &CompactionConfig {
                    context_window: 1_000,
                    reserve_tokens: 100,
                    keep_recent_tokens: 100,
                },
                &summarizer,
            )
            .await
            .expect("auto compact")
            .expect("compaction ran");

        let details = session
            .entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { details, .. } => Some(details.clone()),
                _ => None,
            })
            .flatten()
            .expect("details populated");
        assert_eq!(details.read_files, vec!["src/a.rs"]);
        assert_eq!(details.modified_files, vec!["src/b.rs"]);

        // Summary text should also carry the tagged blocks so
        // resumed sessions see the paths even without the
        // structured field.
        let summary = session
            .entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("summary present");
        assert!(summary.contains("<read-files>"), "{summary}");
        assert!(summary.contains("src/a.rs"), "{summary}");
        assert!(summary.contains("<modified-files>"), "{summary}");
    }

    /// Test-only summarizer that emits different prose for each
    /// call so we can verify the split-turn join happened.
    struct PerCallSummarizer {
        call_index: std::sync::Mutex<usize>,
        responses: Vec<String>,
    }

    impl PerCallSummarizer {
        fn with_responses(responses: Vec<&str>) -> Self {
            Self {
                call_index: std::sync::Mutex::new(0),
                responses: responses.into_iter().map(str::to_string).collect(),
            }
        }

        fn call_count(&self) -> usize {
            *self.call_index.lock().expect("call count lock")
        }
    }

    #[async_trait]
    impl MessageSummarizer for PerCallSummarizer {
        async fn summarize(
            &self,
            _messages: &[Message],
            _existing_summary: Option<&str>,
        ) -> Result<String> {
            let mut idx = self.call_index.lock().expect("call index lock");
            let response = self
                .responses
                .get(*idx)
                .cloned()
                .unwrap_or_else(|| "[no more responses]".into());
            *idx += 1;
            Ok(response)
        }
    }

    #[tokio::test]
    async fn build_context_surfaces_compaction_details_after_compact() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("proj");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("session");

        session
            .append_message(&user_message(&"a".repeat(3_000), 1))
            .expect("u1");
        session
            .append_message(&assistant_with_tool_calls_and_text(
                &"x".repeat(3_000),
                vec![
                    ("read", serde_json::json!({"path": "src/a.rs"})),
                    ("edit", serde_json::json!({"path": "src/a.rs"})),
                ],
                2,
            ))
            .expect("a1");
        session
            .append_message(&user_message("recent", 3))
            .expect("u2");

        let summarizer = RecordingSummarizer::with_summary("prose");
        session
            .auto_compact(
                &CompactionConfig {
                    context_window: 1_000,
                    reserve_tokens: 100,
                    keep_recent_tokens: 100,
                },
                &summarizer,
            )
            .await
            .expect("compact")
            .expect("ran");

        // Via the SessionContext field.
        let ctx = session.build_context();
        let via_context = ctx
            .compaction_details
            .expect("details surfaced via build_context");
        assert_eq!(via_context.read_files, vec!["src/a.rs"]);
        assert_eq!(via_context.modified_files, vec!["src/a.rs"]);

        // Via the direct accessor.
        let via_helper = session
            .latest_compaction_details()
            .expect("details surfaced via helper");
        assert_eq!(via_helper, via_context);
    }

    #[test]
    fn build_context_on_empty_session_has_no_compaction_details() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("proj");
        fs::create_dir_all(&cwd).expect("cwd");
        let session = SessionManager::new_session(tempdir.path(), &cwd).expect("session");
        let ctx = session.build_context();
        assert!(ctx.compaction_details.is_none());
        assert!(session.latest_compaction_details().is_none());
    }

    #[test]
    fn latest_compaction_details_ignores_entries_without_payload() {
        // Compaction with details=None should yield None from
        // the helper, not some empty CompactionDetails default.
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("proj");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("session");

        let first = session.append_message(&user_message("u", 1)).expect("u1");
        session
            .add_entries(vec![SessionEntry::Compaction {
                base: EntryBase {
                    id: session.generate_id(),
                    parent_id: session.leaf_id().map(str::to_string),
                    timestamp: now_iso8601().expect("ts"),
                },
                summary: "s".into(),
                tokens_before: 1,
                first_kept_entry_id: first,
                details: None,
            }])
            .expect("compaction");

        assert!(
            session.latest_compaction_details().is_none(),
            "helper should not fabricate empty details"
        );
    }

    #[tokio::test]
    async fn auto_compact_with_split_turn_produces_two_summaries_joined() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("proj");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("session");

        // Build a session where the cut lands mid-turn. Sizes
        // are chosen so find_cut_point backs off a3 (small)
        // into the kept slice and leaves u2+a2 on the discarded
        // side — splitting turn 2 at index 4.
        //
        //   [discarded]          [kept]
        //   u1, a1, u2, a2   →   a3
        //               ^^ split turn (starts at u2)
        session
            .append_message(&user_message(&"a".repeat(3_000), 1))
            .expect("u1");
        session
            .append_message(&assistant_message(&"b".repeat(3_000), 2))
            .expect("a1");
        session
            .append_message(&user_message(&"c".repeat(3_000), 3))
            .expect("u2");
        session
            .append_message(&assistant_message(&"d".repeat(3_000), 4))
            .expect("a2");
        session
            .append_message(&assistant_message("tail", 5))
            .expect("a3");

        let summarizer = PerCallSummarizer::with_responses(vec!["MAIN_SUMMARY", "PREFIX_SUMMARY"]);

        session
            .auto_compact(
                &CompactionConfig {
                    context_window: 1_000,
                    reserve_tokens: 50,
                    keep_recent_tokens: 100,
                },
                &summarizer,
            )
            .await
            .expect("compact ok")
            .expect("compaction ran");

        assert_eq!(
            summarizer.call_count(),
            2,
            "split-turn path must call summarizer twice"
        );

        let summary = session
            .entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { summary, .. } => Some(summary.clone()),
                _ => None,
            })
            .expect("summary");
        assert!(summary.contains("MAIN_SUMMARY"), "{summary}");
        assert!(summary.contains("PREFIX_SUMMARY"), "{summary}");
        assert!(summary.contains("---"), "{summary}");
        assert!(
            summary.contains("**Turn Context (split turn):**"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn auto_compact_on_clean_boundary_uses_single_summary_path() {
        // Regression: without a split turn, we must still call
        // the summarizer exactly once.
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("proj");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("session");

        session
            .append_message(&user_message(&"a".repeat(3_000), 1))
            .expect("u1");
        session
            .append_message(&assistant_message(&"b".repeat(3_000), 2))
            .expect("a1");
        session
            .append_message(&user_message("short prompt", 3))
            .expect("u2");

        let summarizer =
            PerCallSummarizer::with_responses(vec!["ONLY_SUMMARY", "UNEXPECTED_SECOND"]);

        session
            .auto_compact(
                &CompactionConfig {
                    context_window: 1_000,
                    reserve_tokens: 50,
                    keep_recent_tokens: 50,
                },
                &summarizer,
            )
            .await
            .expect("compact ok")
            .expect("compaction ran");

        assert_eq!(summarizer.call_count(), 1);
    }

    #[tokio::test]
    async fn auto_compact_omits_details_when_no_tracked_tools_fired() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("project");
        fs::create_dir_all(&cwd).expect("cwd");
        let mut session = SessionManager::new_session(tempdir.path(), &cwd).expect("new session");
        session
            .append_message(&user_message(&"a".repeat(3_000), 1))
            .expect("append first");
        session
            .append_message(&assistant_message(&"b".repeat(3_000), 2))
            .expect("append assistant");
        session
            .append_message(&user_message("recent prompt", 3))
            .expect("append recent");

        let summarizer = RecordingSummarizer::with_summary("Prose summary.");
        session
            .auto_compact(
                &CompactionConfig {
                    context_window: 1_000,
                    reserve_tokens: 100,
                    keep_recent_tokens: 100,
                },
                &summarizer,
            )
            .await
            .expect("auto compact")
            .expect("compaction ran");

        let details = session
            .entries
            .iter()
            .find_map(|entry| match entry {
                SessionEntry::Compaction { details, .. } => Some(details.clone()),
                _ => None,
            })
            .expect("compaction entry");
        assert!(
            details.is_none(),
            "details should be None when no tracked tools ran: {details:?}"
        );
    }

    #[test]
    fn single_open_succeeds() {
        let sessions_dir = tempdir().expect("tempdir");
        let cwd = tempdir().expect("cwd tempdir");
        let session = SessionManager::new_session(sessions_dir.path(), cwd.path())
            .expect("first open succeeds");
        assert!(session.path().exists());
    }

    #[test]
    fn second_open_same_file_fails_with_already_open() {
        let sessions_dir = tempdir().expect("tempdir");
        let cwd = tempdir().expect("cwd tempdir");
        let first = SessionManager::new_session(sessions_dir.path(), cwd.path())
            .expect("first open succeeds");
        let path = first.path().to_path_buf();

        match SessionManager::open_session(&path) {
            Ok(_) => panic!("second open must fail while first holds the lock"),
            Err(err) => {
                let cause = err.chain().find_map(|e| e.downcast_ref::<SessionError>());
                assert!(
                    matches!(cause, Some(SessionError::AlreadyOpen(p)) if p == &path),
                    "expected AlreadyOpen({path:?}), got chain {err:?}"
                );
            }
        }
    }

    #[test]
    fn second_open_after_first_dropped_succeeds() {
        let sessions_dir = tempdir().expect("tempdir");
        let cwd = tempdir().expect("cwd tempdir");
        let first = SessionManager::new_session(sessions_dir.path(), cwd.path())
            .expect("first open succeeds");
        let path = first.path().to_path_buf();
        drop(first);

        let _second =
            SessionManager::open_session(&path).expect("reopen after drop should succeed");
    }

    #[test]
    fn write_then_reopen_sees_all_entries() {
        let sessions_dir = tempdir().expect("tempdir");
        let cwd = tempdir().expect("cwd tempdir");
        let mut first = SessionManager::new_session(sessions_dir.path(), cwd.path())
            .expect("first open succeeds");
        let path = first.path().to_path_buf();
        let id = first
            .append_message(&Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: 0,
            }))
            .expect("append message");
        first.flush().expect("flush");
        drop(first);

        let second = SessionManager::open_session(&path).expect("reopen");
        assert!(second.by_id.contains_key(&id));
    }
}
