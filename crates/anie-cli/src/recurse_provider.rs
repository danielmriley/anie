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
use anie_protocol::Message;

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
            RecurseScope::MessageGrep { .. } => Err(anyhow!(
                "RecurseScope::MessageGrep is not yet implemented (planned for rlm/06.x)"
            )),
            RecurseScope::ToolResult { .. } => Err(anyhow!(
                "RecurseScope::ToolResult is not yet implemented (planned for rlm/06.x)"
            )),
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

    /// MessageGrep, ToolResult, File: not yet implemented.
    /// Each returns a typed error mentioning the scope kind
    /// and the milestone it ships in. The recurse tool will
    /// route these to the model as tool errors so it can
    /// adapt strategy (e.g., fall back to MessageRange).
    #[tokio::test]
    async fn unimplemented_scopes_error_with_clear_text() {
        let provider = provider_with_context(Vec::new());

        let cases: Vec<RecurseScope> = vec![
            RecurseScope::MessageGrep {
                pattern: "weather".into(),
            },
            RecurseScope::ToolResult {
                tool_call_id: "call_abc".into(),
            },
            RecurseScope::File {
                path: "/tmp/whatever".into(),
            },
        ];
        for scope in cases {
            let kind = scope.kind();
            let err = provider
                .resolve(&scope)
                .await
                .expect_err("unimplemented scope should error");
            let msg = err.to_string();
            assert!(
                msg.contains("not yet implemented"),
                "error for {kind} should say 'not yet implemented'; got: {msg}",
            );
        }
    }
}
