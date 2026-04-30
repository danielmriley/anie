//! Background summarization worker.
//!
//! Phase F of `docs/rlm_2026-04-29/06_phased_implementation.md`.
//!
//! When the [`ContextVirtualizationPolicy`] archives a
//! message into the [`ExternalContext`], it can enqueue a
//! summarization request through this module's mpsc channel.
//! A long-lived background tokio task receives the requests,
//! generates a summary via the configured [`Summarizer`], and
//! writes it back to the store via `ExternalContext::set_summary`.
//!
//! The recurse tool can then surface either the full message
//! body or the (much smaller) summary, and the reranker can
//! prefer summaries when paging content back in for the
//! current turn — fitting more relevant matches under the
//! budget.
//!
//! Default `Summarizer` implementation: [`HeadTruncationSummarizer`].
//! It keeps the first N characters of the first text block
//! and appends an ellipsis. Cheap, deterministic, and a
//! useful baseline when full content exceeds the relevance
//! budget. A future commit will plug in an LLM-driven
//! summarizer (one-off provider call) without touching the
//! trait surface.
//!
//! Lifecycle:
//! 1. Controller spawns the worker on rlm-mode init,
//!    handing it `Arc<RwLock<ExternalContext>>` plus an
//!    `Arc<dyn Summarizer>`.
//! 2. Policy enqueues `SummaryRequest { id, message }`
//!    after archive (only for messages above a configurable
//!    size threshold — small messages aren't worth
//!    summarizing).
//! 3. Worker pulls from the channel, calls the summarizer,
//!    writes the result with `set_summary`.
//! 4. Worker exits when the sender is dropped (controller
//!    teardown).

use std::sync::Arc;

use anie_protocol::{ContentBlock, Message};
use async_trait::async_trait;
use tokio::sync::{RwLock, mpsc};

use crate::external_context::{ExternalContext, MessageId};

/// Soft cap for summary length in characters. The default
/// summarizer truncates to this; a real LLM summarizer
/// should target the same.
pub(crate) const SUMMARY_MAX_CHARS: usize = 400;

/// Threshold (token estimate) below which messages aren't
/// worth summarizing. Avoids a flood of tiny summaries that
/// would just duplicate the original.
pub(crate) const SUMMARIZE_MIN_TOKENS: u64 = 200;

/// Request sent to the background worker.
#[derive(Debug, Clone)]
pub(crate) struct SummaryRequest {
    /// `ExternalContext` ID to summarize and update.
    pub id: MessageId,
    /// The message to summarize. Cloned at enqueue time so
    /// the worker doesn't need to take a read lock on the
    /// store.
    pub message: Message,
}

/// Strategy for producing a single summary string from a
/// message. Implementations may be cheap (head truncation,
/// keyword extraction) or expensive (LLM call). The trait
/// exists so the controller can swap policies without
/// touching the worker.
#[async_trait]
pub(crate) trait Summarizer: Send + Sync {
    /// Generate a summary string for `message`. Errors are
    /// logged at warn-level and the worker proceeds —
    /// summaries are best-effort and never block the policy.
    async fn summarize(&self, message: &Message) -> Result<String, String>;
}

/// Baseline summarizer: keep the first text block, truncate
/// to `SUMMARY_MAX_CHARS` characters, append an ellipsis if
/// truncated. Useful starting point and a well-defined
/// fallback for LLM-summarizer failures. Future work can
/// plug in a real model-driven summarizer.
pub(crate) struct HeadTruncationSummarizer;

#[async_trait]
impl Summarizer for HeadTruncationSummarizer {
    async fn summarize(&self, message: &Message) -> Result<String, String> {
        let text = first_text(message).unwrap_or("");
        if text.is_empty() {
            return Err("message has no text content to summarize".into());
        }
        let count = text.chars().count();
        if count <= SUMMARY_MAX_CHARS {
            return Ok(text.to_string());
        }
        let mut buf: String = text
            .chars()
            .take(SUMMARY_MAX_CHARS.saturating_sub(1))
            .collect();
        buf.push('…');
        Ok(buf)
    }
}

fn first_text(m: &Message) -> Option<&str> {
    let blocks = match m {
        Message::User(u) => &u.content[..],
        Message::Assistant(a) => &a.content[..],
        Message::ToolResult(t) => &t.content[..],
        Message::Custom(_) => return None,
    };
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            return Some(text.as_str());
        }
    }
    None
}

/// Spawn the background worker. Returns the sender end of
/// the request channel; the caller stashes it on the policy
/// so eviction can enqueue. The receiver is moved into the
/// spawned task and dropped on teardown.
///
/// The worker runs until the sender is closed (every
/// `Sender` clone dropped). When the controller tears down
/// the run, dropping the sender drains the channel and the
/// worker exits cleanly.
pub(crate) fn spawn_worker(
    summarizer: Arc<dyn Summarizer>,
    external: Arc<RwLock<ExternalContext>>,
) -> mpsc::Sender<SummaryRequest> {
    // Bounded channel: high-water mark of 64 pending
    // summarize requests. If the worker falls behind, the
    // policy's `try_send` skips the enqueue rather than
    // blocking the model turn.
    let (tx, mut rx) = mpsc::channel::<SummaryRequest>(64);
    tokio::spawn(async move {
        while let Some(SummaryRequest { id, message }) = rx.recv().await {
            match summarizer.summarize(&message).await {
                Ok(summary) => {
                    let mut store = external.write().await;
                    store.set_summary(id, summary);
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        message_id = id,
                        "background summarizer failed; skipping entry"
                    );
                }
            }
        }
    });
    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{
        AssistantMessage, ContentBlock, StopReason, ToolResultMessage, Usage, UserMessage,
        now_millis,
    };

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: now_millis(),
        })
    }

    fn tool_result_message(body: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_x".into(),
            tool_name: "web_read".into(),
            content: vec![ContentBlock::Text { text: body.into() }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: now_millis(),
        })
    }

    fn assistant_no_text() -> Message {
        Message::Assistant(AssistantMessage {
            content: Vec::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "test".into(),
            model: "test".into(),
            timestamp: now_millis(),
            reasoning_details: None,
        })
    }

    /// HeadTruncationSummarizer: short input passes through
    /// unchanged; long input gets truncated with an ellipsis.
    #[tokio::test]
    async fn head_truncation_truncates_long_text() {
        let summarizer = HeadTruncationSummarizer;
        let short = summarizer.summarize(&user_message("hello")).await.unwrap();
        assert_eq!(short, "hello");

        let long_text: String = "a".repeat(SUMMARY_MAX_CHARS + 200);
        let long = summarizer
            .summarize(&user_message(&long_text))
            .await
            .unwrap();
        assert_eq!(long.chars().count(), SUMMARY_MAX_CHARS);
        assert!(long.ends_with('…'));
    }

    /// Tool results route through `first_text` correctly.
    #[tokio::test]
    async fn head_truncation_handles_tool_results() {
        let summarizer = HeadTruncationSummarizer;
        let summary = summarizer
            .summarize(&tool_result_message("page contents"))
            .await
            .unwrap();
        assert_eq!(summary, "page contents");
    }

    /// Empty content returns an error rather than an empty
    /// summary — caller (the worker) logs and skips.
    #[tokio::test]
    async fn head_truncation_errors_on_empty_content() {
        let summarizer = HeadTruncationSummarizer;
        let result = summarizer.summarize(&assistant_no_text()).await;
        assert!(result.is_err(), "expected Err, got {result:?}");
    }

    /// Worker integration: enqueueing a request results in
    /// the store gaining a summary for that ID.
    #[tokio::test]
    async fn worker_writes_summary_back_to_store() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // Pre-populate the store with one tool result.
        let id = store.write().await.push(tool_result_message("body"));
        let summarizer: Arc<dyn Summarizer> = Arc::new(HeadTruncationSummarizer);
        let tx = spawn_worker(summarizer, Arc::clone(&store));
        tx.send(SummaryRequest {
            id,
            message: tool_result_message("body"),
        })
        .await
        .unwrap();
        // Drop the sender to signal teardown; the worker
        // drains and exits.
        drop(tx);
        // Give the worker a moment to process. In practice
        // this completes in microseconds.
        for _ in 0..50 {
            if store.read().await.get_summary(id).is_some() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(2)).await;
        }
        let summary = store
            .read()
            .await
            .get_summary(id)
            .map(str::to_string)
            .expect("summary written");
        assert_eq!(summary, "body");
    }
}
