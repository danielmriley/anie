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

use anie_protocol::Message;
use anie_session::{SessionContext, SessionInfo, SessionManager};

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
}
