//! Controller-side [`ContextProvider`] implementation.
//!
//! Resolves a [`RecurseScope`] against an indexed
//! [`ExternalContext`] view of the parent run's accumulated
//! messages, plus filesystem access for `RecurseScope::File`.
//! All four scope kinds are implemented (`MessageRange`,
//! `MessageGrep`, `ToolResult`, `File`).
//!
//! Phase B (`docs/rlm_2026-04-29/06_phased_implementation.md`)
//! switched the underlying view from a raw `Vec<Message>` to
//! [`ExternalContext`], which carries `by_tool_call_id` and
//! `by_kind` indexes. The `ToolResult` scope is now an O(1)
//! hash lookup; `MessageGrep` is still a linear scan but
//! over the store's typed iterator.
//!
//! Plan: `docs/rlm_2026-04-29/02_recurse_tool.md` and
//! `docs/rlm_2026-04-29/06_phased_implementation.md` Phase A
//! and B.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use tokio::sync::RwLock;

use anie_agent::{ContextProvider, RecurseScope};
use anie_protocol::{ContentBlock, Message, UserMessage, now_millis};

use crate::external_context::ExternalContext;

/// Maximum file size the `File` scope will load. Mirrors the
/// 50 KiB cap that the `read` tool uses for normal file
/// reads (`anie_tools::shared::MAX_READ_BYTES`); larger files
/// surface as an explicit "too large" error so the model
/// retries with `read` + a focused range, or with a
/// different scope kind.
const FILE_SCOPE_MAX_BYTES: u64 = 50 * 1024;

/// True iff `re` matches any text content in `message`. Walks
/// every text-bearing block — assistant text, user text,
/// tool-result text — so a recurse-with-grep can locate
/// content regardless of which role originally produced it.
/// Non-text blocks (tool calls, images, thinking signatures)
/// are ignored.
fn message_matches_regex(message: &Message, re: &regex::Regex) -> bool {
    let blocks: &[ContentBlock] = match message {
        Message::User(u) => &u.content,
        Message::Assistant(a) => &a.content,
        Message::ToolResult(t) => &t.content,
        Message::Custom(_) => return false,
    };
    blocks.iter().any(|b| match b {
        ContentBlock::Text { text } => re.is_match(text),
        ContentBlock::Thinking { thinking, .. } => re.is_match(thinking),
        _ => false,
    })
}

/// Controller-side resolver for [`RecurseScope`] values.
///
/// Holds a shared [`ExternalContext`] — the indexed store
/// the controller populates at run start. The controller
/// updates the store after each REPL `Print` step (Phase C
/// of Plan 06 wires this up); for now the store is a
/// snapshot taken at run start. Reads are async because the
/// `File` scope blocks on I/O; the lock is
/// `tokio::sync::RwLock` so multiple concurrent recurse
/// calls can read the store in parallel.
pub(crate) struct ControllerContextProvider {
    store: Arc<RwLock<ExternalContext>>,
}

impl ControllerContextProvider {
    /// Construct a provider around a shared external store.
    pub(crate) fn new(store: Arc<RwLock<ExternalContext>>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ContextProvider for ControllerContextProvider {
    async fn resolve(&self, scope: &RecurseScope) -> Result<Vec<Message>> {
        match scope {
            RecurseScope::MessageRange { start, end } => {
                let store = self.store.read().await;
                store
                    .range(*start, *end)
                    .map_err(|msg| anyhow!("RecurseScope::MessageRange {msg}"))
            }
            RecurseScope::MessageGrep { pattern } => {
                let re = regex::Regex::new(pattern).map_err(|e| {
                    anyhow!("RecurseScope::MessageGrep invalid regex `{pattern}`: {e}")
                })?;
                let store = self.store.read().await;
                let matched: Vec<Message> = store
                    .iter()
                    .filter(|m| message_matches_regex(m, &re))
                    .cloned()
                    .collect();
                Ok(matched)
            }
            RecurseScope::ToolResult { tool_call_id } => {
                let store = self.store.read().await;
                // O(1) lookup via the by_tool_call_id index.
                let id = store.find_by_tool_call_id(tool_call_id).ok_or_else(|| {
                    anyhow!(
                        "RecurseScope::ToolResult tool_call_id `{tool_call_id}` not found in parent context"
                    )
                })?;
                // The store guarantees `find_by_tool_call_id` ID
                // is in-range, so unwrap is safe — but we still
                // route through `Option` for explicitness.
                let message = store.get_by_id(id).cloned().ok_or_else(|| {
                    anyhow!("RecurseScope::ToolResult store inconsistency: id {id} not found")
                })?;
                Ok(vec![message])
            }
            RecurseScope::File { path } => {
                let metadata = tokio::fs::metadata(path)
                    .await
                    .map_err(|e| anyhow!("RecurseScope::File could not stat `{path}`: {e}"))?;
                if !metadata.is_file() {
                    return Err(anyhow!("RecurseScope::File `{path}` is not a regular file"));
                }
                if metadata.len() > FILE_SCOPE_MAX_BYTES {
                    return Err(anyhow!(
                        "RecurseScope::File `{path}` exceeds the {FILE_SCOPE_MAX_BYTES}-byte limit ({} bytes); use the `read` tool with a focused offset/limit instead",
                        metadata.len(),
                    ));
                }
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|e| anyhow!("RecurseScope::File could not read `{path}`: {e}"))?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                // Wrap the file as a single User message so the
                // sub-agent's prompt format is uniform across
                // scope kinds. The header names the source path
                // so the sub-agent knows where the content came
                // from when it composes its answer.
                let body = format!("[file: {path}]\n\n{text}");
                Ok(vec![Message::User(UserMessage {
                    content: vec![ContentBlock::Text { text: body }],
                    timestamp: now_millis(),
                })])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{ContentBlock, UserMessage, now_millis};

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: now_millis(),
        })
    }

    fn provider_with_context(messages: Vec<Message>) -> ControllerContextProvider {
        ControllerContextProvider::new(Arc::new(RwLock::new(ExternalContext::from_messages(
            messages,
        ))))
    }

    /// MessageRange resolves the half-open `[start, end)`
    /// range and clones the messages out of the view.
    #[tokio::test]
    async fn message_range_resolves_to_subset() {
        let provider = provider_with_context(vec![
            user_message("zero"),
            user_message("one"),
            user_message("two"),
            user_message("three"),
            user_message("four"),
        ]);
        let resolved = provider
            .resolve(&RecurseScope::MessageRange { start: 1, end: 3 })
            .await
            .expect("resolve ok");
        assert_eq!(resolved.len(), 2);
        let texts: Vec<String> = resolved
            .into_iter()
            .filter_map(|m| match m {
                Message::User(u) => match &u.content[0] {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["one".to_string(), "two".to_string()]);
    }

    /// Empty range `[N, N)` is valid and resolves to an
    /// empty Vec — useful for callers that want to express
    /// "no messages, just my sub-query."
    #[tokio::test]
    async fn message_range_empty_is_ok() {
        let provider = provider_with_context(vec![user_message("one"), user_message("two")]);
        let resolved = provider
            .resolve(&RecurseScope::MessageRange { start: 1, end: 1 })
            .await
            .expect("resolve ok");
        assert!(resolved.is_empty());
    }

    /// Whole-context range resolves to a copy of every
    /// message.
    #[tokio::test]
    async fn message_range_full_returns_clone() {
        let messages = vec![user_message("a"), user_message("b"), user_message("c")];
        let provider = provider_with_context(messages.clone());
        let resolved = provider
            .resolve(&RecurseScope::MessageRange { start: 0, end: 3 })
            .await
            .expect("resolve ok");
        assert_eq!(resolved, messages);
    }

    /// `start > end` is an invalid range; resolver returns
    /// an error rather than silently swapping or returning
    /// empty.
    #[tokio::test]
    async fn message_range_inverted_errors() {
        let provider = provider_with_context(vec![user_message("only")]);
        let err = provider
            .resolve(&RecurseScope::MessageRange { start: 5, end: 2 })
            .await
            .expect_err("inverted range should error");
        let msg = err.to_string();
        assert!(
            msg.contains("start (5) > end (2)"),
            "error should name the offending bounds; got: {msg}",
        );
    }

    /// `end > context.len()` is out of bounds; resolver
    /// errors rather than truncating silently.
    #[tokio::test]
    async fn message_range_end_out_of_bounds_errors() {
        let provider = provider_with_context(vec![user_message("only")]);
        let err = provider
            .resolve(&RecurseScope::MessageRange { start: 0, end: 99 })
            .await
            .expect_err("out-of-bounds end should error");
        let msg = err.to_string();
        assert!(
            msg.contains("end (99) exceeds context length (1)"),
            "error should name both the bound and the actual length; got: {msg}",
        );
    }

    /// File scope reads a small file from disk and wraps the
    /// contents in a single User message tagged with the
    /// source path.
    #[tokio::test]
    async fn file_scope_reads_small_file() {
        use std::io::Write as _;
        use tempfile::NamedTempFile;
        let mut tmp = NamedTempFile::new().expect("tmp");
        writeln!(tmp, "hello from disk").expect("write");
        let path = tmp.path().to_string_lossy().into_owned();
        let provider = provider_with_context(Vec::new());
        let resolved = provider
            .resolve(&RecurseScope::File { path: path.clone() })
            .await
            .expect("resolve ok");
        assert_eq!(resolved.len(), 1);
        match &resolved[0] {
            Message::User(u) => {
                let ContentBlock::Text { text } = &u.content[0] else {
                    panic!("expected text content");
                };
                assert!(text.contains(&format!("[file: {path}]")));
                assert!(text.contains("hello from disk"));
            }
            other => panic!("expected User message, got {other:?}"),
        }
    }

    /// Files larger than the cap surface as a typed error
    /// rather than loading silently. The error message
    /// suggests `read` as the right tool for focused reads.
    #[tokio::test]
    async fn file_scope_rejects_oversize_file() {
        use std::io::Write as _;
        use tempfile::NamedTempFile;
        let mut tmp = NamedTempFile::new().expect("tmp");
        let big = vec![b'x'; (FILE_SCOPE_MAX_BYTES as usize) + 100];
        tmp.write_all(&big).expect("write");
        let provider = provider_with_context(Vec::new());
        let err = provider
            .resolve(&RecurseScope::File {
                path: tmp.path().to_string_lossy().into_owned(),
            })
            .await
            .expect_err("oversize file should error");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds") && msg.contains("byte limit"),
            "error should name the limit; got: {msg}",
        );
    }

    #[tokio::test]
    async fn file_scope_missing_path_errors() {
        let provider = provider_with_context(Vec::new());
        let err = provider
            .resolve(&RecurseScope::File {
                path: "/nonexistent/path/whatever".into(),
            })
            .await
            .expect_err("missing path should error");
        assert!(err.to_string().contains("could not stat"));
    }

    #[tokio::test]
    async fn file_scope_directory_errors() {
        use tempfile::tempdir;
        let dir = tempdir().expect("tempdir");
        let provider = provider_with_context(Vec::new());
        let err = provider
            .resolve(&RecurseScope::File {
                path: dir.path().to_string_lossy().into_owned(),
            })
            .await
            .expect_err("directory should error");
        assert!(err.to_string().contains("not a regular file"));
    }

    /// ToolResult resolves a single ToolResultMessage by
    /// `tool_call_id`. The match is exact; the resolver
    /// returns a Vec for shape uniformity with the other
    /// scopes (always exactly 0 or 1 entry in the success
    /// case — empty triggers an error so the model knows the
    /// id was wrong).
    #[tokio::test]
    async fn tool_result_resolves_by_call_id() {
        use anie_protocol::{ToolResultMessage, now_millis};
        let tool_result = Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_xyz".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::Text {
                text: "hello\n".into(),
            }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: now_millis(),
        });
        let provider = provider_with_context(vec![user_message("before"), tool_result.clone()]);
        let resolved = provider
            .resolve(&RecurseScope::ToolResult {
                tool_call_id: "call_xyz".into(),
            })
            .await
            .expect("resolve ok");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0], tool_result);
    }

    #[tokio::test]
    async fn tool_result_unknown_id_errors() {
        let provider = provider_with_context(vec![user_message("anything")]);
        let err = provider
            .resolve(&RecurseScope::ToolResult {
                tool_call_id: "missing".into(),
            })
            .await
            .expect_err("unknown id should error");
        let msg = err.to_string();
        assert!(
            msg.contains("missing") && msg.contains("not found"),
            "error should name the missing id; got: {msg}",
        );
    }

    /// MessageGrep returns every message whose any text block
    /// matches the regex.
    #[tokio::test]
    async fn message_grep_returns_matching_messages() {
        let provider = provider_with_context(vec![
            user_message("the weather in paris is rainy"),
            user_message("totally unrelated thought"),
            user_message("today's WEATHER forecast for nyc"),
            user_message("another unrelated thing"),
        ]);
        let resolved = provider
            .resolve(&RecurseScope::MessageGrep {
                pattern: "(?i)weather".into(),
            })
            .await
            .expect("resolve ok");
        assert_eq!(resolved.len(), 2);
    }

    /// MessageGrep with no matches returns an empty Vec, not
    /// an error — the model can interpret "no results" cleanly
    /// and either retry with a different pattern or move on.
    #[tokio::test]
    async fn message_grep_no_matches_returns_empty() {
        let provider = provider_with_context(vec![user_message("hello"), user_message("world")]);
        let resolved = provider
            .resolve(&RecurseScope::MessageGrep {
                pattern: "absent_pattern".into(),
            })
            .await
            .expect("resolve ok");
        assert!(resolved.is_empty());
    }

    /// Invalid regex surfaces as a typed error naming the
    /// pattern.
    #[tokio::test]
    async fn message_grep_invalid_regex_errors() {
        let provider = provider_with_context(vec![user_message("doesn't matter")]);
        let err = provider
            .resolve(&RecurseScope::MessageGrep {
                pattern: "[unbalanced".into(),
            })
            .await
            .expect_err("invalid regex should error");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid regex"),
            "error should mention regex invalidity; got: {msg}",
        );
    }
}
