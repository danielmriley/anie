//! Controller-side [`ContextProvider`] implementation.
//!
//! Resolves a [`RecurseScope`] against an `Arc<RwLock<Vec<Message>>>`
//! view of the parent run's active context, plus (in later
//! commits) filesystem access for `RecurseScope::File`.
//!
//! This commit (`rlm/03`) ships [`RecurseScope::MessageRange`]
//! resolution only. The other scope kinds (`MessageGrep`,
//! `ToolResult`, `File`) return a typed "not yet
//! implemented" error so the recurse tool — which will land
//! in `rlm/04` — can route them through to the model as a
//! tool error rather than silently mis-handling them. Each
//! gets its own commit (`rlm/06.x`).
//!
//! Plan: `docs/rlm_2026-04-29/02_recurse_tool.md` and
//! `docs/rlm_2026-04-29/06_phased_implementation.md` Phase A.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use tokio::sync::RwLock;

use anie_agent::{ContextProvider, RecurseScope};
use anie_protocol::{ContentBlock, Message};

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
/// Holds a *view* of the parent run's context — not an
/// owning copy. The controller updates the view after each
/// REPL `Print` step (Phase C of Plan 06 wires this up); for
/// now the view is whatever a caller threads in. Reads are
/// async because future scope kinds (`File`) will block on
/// I/O; the lock is `tokio::sync::RwLock` so multiple
/// concurrent recurse calls can read the view in parallel.
///
/// `#[allow(dead_code)]`: this commit (`rlm/03`) ships the
/// resolver as a standalone, fully-tested unit. The recurse
/// tool that consumes it lands in `rlm/04`. Tests in this
/// file exercise the resolver directly; production code
/// doesn't reference it yet, hence the lint allowance until
/// commit 4 wires it up.
#[allow(dead_code)]
pub(crate) struct ControllerContextProvider {
    context_view: Arc<RwLock<Vec<Message>>>,
}

#[allow(dead_code)]
impl ControllerContextProvider {
    /// Construct a provider around a shared context view.
    pub(crate) fn new(context_view: Arc<RwLock<Vec<Message>>>) -> Self {
        Self { context_view }
    }
}

#[async_trait]
impl ContextProvider for ControllerContextProvider {
    async fn resolve(&self, scope: &RecurseScope) -> Result<Vec<Message>> {
        match scope {
            RecurseScope::MessageRange { start, end } => {
                let context = self.context_view.read().await;
                if start > end {
                    return Err(anyhow!(
                        "RecurseScope::MessageRange start ({start}) > end ({end})"
                    ));
                }
                if *end > context.len() {
                    return Err(anyhow!(
                        "RecurseScope::MessageRange end ({end}) exceeds context length ({})",
                        context.len()
                    ));
                }
                Ok(context[*start..*end].to_vec())
            }
            RecurseScope::MessageGrep { pattern } => {
                let re = regex::Regex::new(pattern).map_err(|e| {
                    anyhow!("RecurseScope::MessageGrep invalid regex `{pattern}`: {e}")
                })?;
                let context = self.context_view.read().await;
                let matched: Vec<Message> = context
                    .iter()
                    .filter(|m| message_matches_regex(m, &re))
                    .cloned()
                    .collect();
                Ok(matched)
            }
            RecurseScope::ToolResult { tool_call_id } => {
                let context = self.context_view.read().await;
                let matched: Vec<Message> = context
                    .iter()
                    .filter(|m| match m {
                        Message::ToolResult(t) => &t.tool_call_id == tool_call_id,
                        _ => false,
                    })
                    .cloned()
                    .collect();
                if matched.is_empty() {
                    return Err(anyhow!(
                        "RecurseScope::ToolResult tool_call_id `{tool_call_id}` not found in parent context"
                    ));
                }
                Ok(matched)
            }
            RecurseScope::File { .. } => Err(anyhow!(
                "RecurseScope::File is not yet implemented (planned for rlm/06.x)"
            )),
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
        ControllerContextProvider::new(Arc::new(RwLock::new(messages)))
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

    /// File is not yet implemented; returns a typed error.
    #[tokio::test]
    async fn unimplemented_file_scope_errors_with_clear_text() {
        let provider = provider_with_context(Vec::new());
        let err = provider
            .resolve(&RecurseScope::File {
                path: "/tmp/whatever".into(),
            })
            .await
            .expect_err("file scope unimplemented");
        assert!(err.to_string().contains("not yet implemented"));
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
