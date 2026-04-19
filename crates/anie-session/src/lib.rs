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
}

impl SessionContext {
    /// Create an empty session context.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            messages: Vec::new(),
            thinking_level: None,
            model: None,
        }
    }
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
    by_id: HashMap<String, usize>,
    id_set: HashSet<String>,
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

        let session_id = generate_unique_id(&existing);
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let header = SessionHeader {
            entry_type: "session".into(),
            version: 1,
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
            id_set: HashSet::new(),
            leaf_id: None,
            file_handle,
        })
    }

    /// Open an existing session file and rebuild its in-memory indexes.
    pub fn open_session(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read session file {}", path.display()))?;
        let (header, entries) = parse_session_file(&content)?;
        let mut by_id = HashMap::new();
        let mut id_set = HashSet::new();
        let mut leaf_id = None;

        for (index, entry) in entries.iter().enumerate() {
            let id = entry.base().id.clone();
            by_id.insert(id.clone(), index);
            id_set.insert(id.clone());
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
            id_set,
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
        if !self.id_set.contains(from_entry_id) {
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
                && !self.id_set.contains(parent_id)
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
            self.id_set.insert(id.clone());
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
                ..
            } => Some((summary.clone(), first_kept_entry_id.clone())),
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
        if let Some((summary, _)) = &latest_compaction {
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
            if let Some((_, first_kept_entry_id)) = &latest_compaction
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

        SessionContext {
            messages,
            thinking_level,
            model,
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
        generate_unique_id(&self.id_set)
    }

    async fn compact_internal(
        &mut self,
        tokens_before: u64,
        keep_recent_tokens: u64,
        summarizer: &dyn MessageSummarizer,
    ) -> Result<Option<CompactionResult>> {
        let context = self.build_context();
        let Ok((discard, _keep, first_kept_entry_id)) =
            find_cut_point(&context.messages, keep_recent_tokens)
        else {
            return Ok(None);
        };

        let source_messages = discard
            .iter()
            .map(|message| message.message.clone())
            .collect::<Vec<_>>();
        let existing_summary = self.latest_compaction_summary();
        let summary = summarizer
            .summarize(&source_messages, existing_summary.as_deref())
            .await?;

        let entry = SessionEntry::Compaction {
            base: EntryBase {
                id: self.generate_id(),
                parent_id: self.leaf_id.clone(),
                timestamp: now_iso8601()?,
            },
            summary: summary.clone(),
            tokens_before,
            first_kept_entry_id: first_kept_entry_id.clone(),
        };
        self.add_entries(vec![entry])?;

        Ok(Some(CompactionResult {
            summary,
            tokens_before,
            first_kept_entry_id,
            messages_discarded: discard.len(),
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
#[must_use]
pub fn estimate_context_tokens(messages: &[SessionContextMessage]) -> u64 {
    messages
        .iter()
        .map(|message| estimate_tokens(&message.message))
        .sum()
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

/// Find the compaction cut point for the provided active-branch context.
pub fn find_cut_point(
    messages: &[SessionContextMessage],
    keep_recent_tokens: u64,
) -> Result<(
    Vec<SessionContextMessage>,
    Vec<SessionContextMessage>,
    String,
)> {
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

    let discard = messages[..cut_index].to_vec();
    let keep = messages[cut_index..].to_vec();
    let first_kept_entry_id = keep[0].entry_id.clone();
    Ok((discard, keep, first_kept_entry_id))
}

fn generate_unique_id(existing: &HashSet<String>) -> String {
    for _ in 0..100 {
        let id = Uuid::new_v4().simple().to_string()[..8].to_string();
        if !existing.contains(&id) {
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
            }])
            .expect("append compaction");

        assert_eq!(
            session.estimate_context_tokens(),
            estimate_context_tokens(&session.build_context().messages)
        );
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

        let (discard, keep, first_kept) = find_cut_point(&messages, 3).expect("cut point");
        assert_eq!(discard.len(), 3);
        assert_eq!(keep.len(), 1);
        assert_eq!(first_kept, "4");
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
