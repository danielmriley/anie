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
//! Two `Summarizer` implementations:
//! - [`LlmSummarizer`] — production default. Issues one
//!   non-tool streaming call against the run's configured
//!   provider/model with a short system prompt asking for
//!   a 3-5 sentence summary. On any failure (timeout,
//!   provider error, empty output) it falls back to head-
//!   truncation so the entry still gets *some* summary.
//! - [`HeadTruncationSummarizer`] — deterministic baseline:
//!   keep the first `SUMMARY_MAX_CHARS` characters of the
//!   first text block. Used as the fallback path inside
//!   `LlmSummarizer` and as a stand-alone option in tests.
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

use std::{sync::Arc, time::Duration};

use anie_protocol::{ContentBlock, Message, UserMessage, now_millis};
use anie_provider::{
    LlmContext, Model, ProviderEvent, ProviderRegistry, RequestOptionsResolver, StreamOptions,
    ThinkingLevel,
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::{RwLock, mpsc};

use crate::external_context::{ExternalContext, MessageId};

/// Default per-call timeout for the LLM-driven summarizer.
/// Keeps a stuck or slow provider call from blocking the
/// worker queue indefinitely. Generous: small models on
/// local Ollama can take 30+s for a 400-char summary, and
/// large MoE models (qwen3.6:latest, 36B) frequently exceed
/// 90s. Operators tune via `ANIE_SUMMARIZER_TIMEOUT_SECS`.
const DEFAULT_LLM_SUMMARIZER_TIMEOUT_SECS: u64 = 180;

/// Read the LLM-summarizer timeout from
/// `ANIE_SUMMARIZER_TIMEOUT_SECS`, falling back to
/// [`DEFAULT_LLM_SUMMARIZER_TIMEOUT_SECS`] when unset or
/// unparseable. Set to a generous value (e.g. 600) for big
/// local models that take minutes per summary.
fn llm_summarizer_timeout_secs() -> u64 {
    std::env::var("ANIE_SUMMARIZER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_LLM_SUMMARIZER_TIMEOUT_SECS)
}

/// System prompt used by the LLM-driven summarizer. Short,
/// directive, model-agnostic. Asks for a 3-5 sentence
/// summary so the result fits comfortably under
/// `SUMMARY_MAX_CHARS` for most providers.
const LLM_SUMMARIZER_SYSTEM_PROMPT: &str = "You produce concise summaries of conversation messages. \
Output a 3-5 sentence summary capturing the most important points: subject, key facts, decisions, URLs, and relevant numbers. \
Output only the summary text — no preamble, no markdown headers, no quoting. Keep it under 400 characters.";

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
/// truncated. For Assistant messages whose only content is
/// `ToolCall` blocks (no text), synthesizes a placeholder
/// summary describing the tool calls instead of erroring —
/// otherwise tool-orchestrating turns would never get
/// summaries and the recurse-by-summary path would miss
/// them.
pub(crate) struct HeadTruncationSummarizer;

#[async_trait]
impl Summarizer for HeadTruncationSummarizer {
    async fn summarize(&self, message: &Message) -> Result<String, String> {
        if let Some(text) = first_text(message) {
            if !text.is_empty() {
                let count = text.chars().count();
                if count <= SUMMARY_MAX_CHARS {
                    return Ok(text.to_string());
                }
                let mut buf: String = text
                    .chars()
                    .take(SUMMARY_MAX_CHARS.saturating_sub(1))
                    .collect();
                buf.push('…');
                return Ok(buf);
            }
        }
        // Fall through: no text body. If it's an Assistant
        // with tool calls, synthesize a placeholder so the
        // turn shows up in summary scope.
        if let Some(synth) = synthesize_tool_call_summary(message) {
            return Ok(synth);
        }
        Err("message has no text content to summarize".into())
    }
}

/// Synthesize a "[assistant called tool X(args)]" line for
/// Assistant messages whose only content is `ToolCall`
/// blocks. Returns `None` for any other message kind.
fn synthesize_tool_call_summary(message: &Message) -> Option<String> {
    let Message::Assistant(assistant) = message else {
        return None;
    };
    let tool_calls: Vec<&anie_protocol::ToolCall> = assistant
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .collect();
    if tool_calls.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for call in tool_calls {
        // Pull a representative arg if we can recognize the
        // tool — same set the ledger uses (web_read.url,
        // web_search.query, bash.command, read.path, etc).
        // Falls through to a bare "name()" otherwise.
        let arg = call
            .arguments
            .get("url")
            .or_else(|| call.arguments.get("query"))
            .or_else(|| call.arguments.get("command"))
            .or_else(|| call.arguments.get("path"))
            .and_then(|v| v.as_str())
            .map(|s| s.chars().take(80).collect::<String>())
            .unwrap_or_default();
        if arg.is_empty() {
            parts.push(format!("{}()", call.name));
        } else {
            parts.push(format!("{}({arg})", call.name));
        }
    }
    let joined = parts.join("; ");
    let header = "[assistant tool calls] ";
    let max_body = SUMMARY_MAX_CHARS.saturating_sub(header.chars().count());
    let body: String = if joined.chars().count() <= max_body {
        joined
    } else {
        let mut clipped: String = joined.chars().take(max_body.saturating_sub(1)).collect();
        clipped.push('…');
        clipped
    };
    Some(format!("{header}{body}"))
}

/// Provider-driven summarizer. Issues a single non-tool
/// streaming call against the run's configured provider /
/// model, asking for a short summary. Falls back to
/// `HeadTruncationSummarizer` semantics on any error
/// (timeout, provider failure, empty output) so the
/// worker still produces *something* — partial progress
/// beats none.
pub(crate) struct LlmSummarizer {
    provider_registry: Arc<ProviderRegistry>,
    model: Model,
    request_options_resolver: Arc<dyn RequestOptionsResolver>,
    /// Optional override for Ollama's `num_ctx` so summary
    /// calls inherit whatever the user set on the parent.
    /// `None` falls back to `Model.context_window`.
    num_ctx_override: Option<u64>,
}

impl LlmSummarizer {
    pub(crate) fn new(
        provider_registry: Arc<ProviderRegistry>,
        model: Model,
        request_options_resolver: Arc<dyn RequestOptionsResolver>,
        num_ctx_override: Option<u64>,
    ) -> Self {
        Self {
            provider_registry,
            model,
            request_options_resolver,
            num_ctx_override,
        }
    }

    async fn summarize_via_llm(&self, message: &Message) -> Result<String, String> {
        let text = first_text(message)
            .ok_or_else(|| "message has no text content to summarize".to_string())?;
        if text.is_empty() {
            return Err("message has no text content to summarize".into());
        }

        // Build a one-shot user-message context with the
        // body to summarize wrapped so the model knows what
        // it's processing.
        let user_prompt = format!("Summarize this message:\n\n{text}");
        let prompt_message = Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: user_prompt }],
            timestamp: now_millis(),
        });
        let prompt_slice = std::slice::from_ref(&prompt_message);

        // Resolve auth + provider routing.
        let resolved = self
            .request_options_resolver
            .resolve(&self.model, prompt_slice)
            .await
            .map_err(|e| format!("resolver: {e}"))?;
        let mut model = self.model.clone();
        if let Some(base_url) = resolved.base_url_override {
            model.base_url = base_url;
        }

        let provider = self
            .provider_registry
            .get(&model.api)
            .ok_or_else(|| format!("no provider registered for {:?}", model.api))?;

        let llm_context = LlmContext {
            system_prompt: LLM_SUMMARIZER_SYSTEM_PROMPT.to_string(),
            messages: provider.convert_messages(prompt_slice),
            tools: Vec::new(),
        };
        let mut options = StreamOptions {
            api_key: resolved.api_key,
            headers: resolved.headers,
            num_ctx_override: self.num_ctx_override,
            thinking: ThinkingLevel::Off,
            ..StreamOptions::default()
        };
        // Cap output tokens to roughly the summary budget.
        // Tokenization is provider-specific, so this is an
        // upper bound — we still trim post-hoc.
        options.max_tokens = Some((SUMMARY_MAX_CHARS as u64) / 2 + 128);

        // Issue the call and consume the stream into a
        // single string. Bound the whole thing in a
        // timeout so a stuck provider can't hang the
        // worker.
        let stream_result = provider.stream(&model, llm_context, options);
        let mut stream = stream_result.map_err(|e| format!("stream init: {e}"))?;
        let collect_fut = async {
            let mut buf = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ProviderEvent::TextDelta(text)) => buf.push_str(&text),
                    Ok(ProviderEvent::Done(assistant)) => {
                        // If we got no deltas (some providers
                        // only deliver content on Done), pull
                        // the final assistant text.
                        if buf.is_empty() {
                            for block in &assistant.content {
                                if let ContentBlock::Text { text } = block {
                                    buf.push_str(text);
                                }
                            }
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("stream: {e}")),
                }
            }
            Ok(buf)
        };
        let timeout_secs = llm_summarizer_timeout_secs();
        let buf = tokio::time::timeout(Duration::from_secs(timeout_secs), collect_fut)
            .await
            .map_err(|_| format!("timed out after {timeout_secs}s"))??;

        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            return Err("provider returned empty summary".into());
        }
        // Defense in depth: cap the summary at
        // SUMMARY_MAX_CHARS in case the model ignored the
        // length instruction.
        if trimmed.chars().count() > SUMMARY_MAX_CHARS {
            let mut clipped: String = trimmed
                .chars()
                .take(SUMMARY_MAX_CHARS.saturating_sub(1))
                .collect();
            clipped.push('…');
            return Ok(clipped);
        }
        Ok(trimmed)
    }
}

#[async_trait]
impl Summarizer for LlmSummarizer {
    async fn summarize(&self, message: &Message) -> Result<String, String> {
        // Try the LLM first; on any failure, fall back to
        // head-truncation so the entry still gets *some*
        // summary. Logged at warn-level so operators can
        // tell why the provider path didn't produce.
        match self.summarize_via_llm(message).await {
            Ok(summary) => Ok(summary),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "LLM summarizer failed; falling back to head truncation",
                );
                HeadTruncationSummarizer.summarize(message).await
            }
        }
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

    /// Empty content (no blocks at all) returns an error
    /// rather than an empty summary — caller (the worker)
    /// logs and skips. Tool-call-only Assistant messages
    /// take a different path: see the synthesizer test
    /// below.
    #[tokio::test]
    async fn head_truncation_errors_on_empty_content() {
        let summarizer = HeadTruncationSummarizer;
        let result = summarizer.summarize(&assistant_no_text()).await;
        assert!(result.is_err(), "expected Err, got {result:?}");
    }

    /// `ANIE_SUMMARIZER_TIMEOUT_SECS` overrides the
    /// default. Verifies env-var parsing + fallback.
    #[test]
    fn llm_summarizer_timeout_reads_env_var() {
        // Use temp_env to scope the env var change.
        temp_env::with_var("ANIE_SUMMARIZER_TIMEOUT_SECS", Some("42"), || {
            assert_eq!(llm_summarizer_timeout_secs(), 42);
        });
        // Unparseable falls back to default.
        temp_env::with_var("ANIE_SUMMARIZER_TIMEOUT_SECS", Some("not-a-number"), || {
            assert_eq!(
                llm_summarizer_timeout_secs(),
                DEFAULT_LLM_SUMMARIZER_TIMEOUT_SECS
            );
        });
        // Unset falls back to default. `with_var_unset`
        // is the typed helper for the unset case (so we
        // don't have to spell out the closure's never-used
        // value-type parameter).
        temp_env::with_var_unset("ANIE_SUMMARIZER_TIMEOUT_SECS", || {
            assert_eq!(
                llm_summarizer_timeout_secs(),
                DEFAULT_LLM_SUMMARIZER_TIMEOUT_SECS
            );
        });
    }

    /// Assistant message whose content is only `ToolCall`
    /// blocks (no text body) gets a synthesized
    /// "[assistant tool calls] name(arg)" summary instead
    /// of erroring. Without this, tool-orchestrating turns
    /// would never get summaries and the recurse-by-summary
    /// scope would silently miss them.
    #[tokio::test]
    async fn head_truncation_synthesizes_tool_call_only_assistants() {
        use anie_protocol::ToolCall;
        let assistant = Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "web_read".into(),
                    arguments: serde_json::json!({"url": "https://example.com/page"}),
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "c2".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "ls -la"}),
                }),
            ],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "test".into(),
            model: "test".into(),
            timestamp: now_millis(),
            reasoning_details: None,
        });
        let summary = HeadTruncationSummarizer
            .summarize(&assistant)
            .await
            .expect("should synthesize");
        assert!(
            summary.starts_with("[assistant tool calls]"),
            "expected synth header; got {summary}"
        );
        assert!(summary.contains("web_read(https://example.com/page)"));
        assert!(summary.contains("bash(ls -la)"));
    }

    /// LlmSummarizer integration: feed a scripted mock
    /// provider that emits two TextDelta chunks and Done;
    /// expect the summary to be the concatenation of the
    /// two chunks (trimmed). Verifies the streaming
    /// consumption + the trim/cap path.
    #[tokio::test]
    async fn llm_summarizer_collects_text_deltas() {
        use anie_provider::{
            ApiKind, CostPerMillion, ModelCompat, ProviderError, ProviderEvent, ProviderRegistry,
            ResolvedRequestOptions,
            mock::{MockProvider, MockStreamScript},
        };

        struct StaticResolver;
        #[async_trait]
        impl RequestOptionsResolver for StaticResolver {
            async fn resolve(
                &self,
                _model: &Model,
                _context: &[Message],
            ) -> Result<ResolvedRequestOptions, ProviderError> {
                Ok(ResolvedRequestOptions::default())
            }
        }

        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text {
                text: " final ".into(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "mock".into(),
            timestamp: now_millis(),
            reasoning_details: None,
        };
        let script = MockStreamScript::new(vec![
            Ok(ProviderEvent::TextDelta("Concise ".into())),
            Ok(ProviderEvent::TextDelta("summary.".into())),
            Ok(ProviderEvent::Done(assistant)),
        ]);

        let mut registry = ProviderRegistry::new();
        registry.register(
            ApiKind::OpenAICompletions,
            Box::new(MockProvider::new(vec![script])),
        );

        let model = Model {
            id: "mock".into(),
            name: "mock".into(),
            provider: "mock".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 8_192,
            max_tokens: 1_024,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        };

        let summarizer =
            LlmSummarizer::new(Arc::new(registry), model, Arc::new(StaticResolver), None);
        let body = "Some long body that needs summarizing for testing.";
        let summary = summarizer
            .summarize(&user_message(body))
            .await
            .expect("summarize ok");
        assert_eq!(summary, "Concise summary.");
    }

    /// LlmSummarizer fallback: when the provider stream
    /// returns an error, summarize() falls back to head-
    /// truncation rather than failing.
    #[tokio::test]
    async fn llm_summarizer_falls_back_on_provider_error() {
        use anie_provider::{
            ApiKind, CostPerMillion, ModelCompat, ProviderError, ProviderRegistry,
            ResolvedRequestOptions,
            mock::{MockProvider, MockStreamScript},
        };

        struct StaticResolver;
        #[async_trait]
        impl RequestOptionsResolver for StaticResolver {
            async fn resolve(
                &self,
                _model: &Model,
                _context: &[Message],
            ) -> Result<ResolvedRequestOptions, ProviderError> {
                Ok(ResolvedRequestOptions::default())
            }
        }

        let script =
            MockStreamScript::from_error(ProviderError::Auth("test-mode forced error".into()));
        let mut registry = ProviderRegistry::new();
        registry.register(
            ApiKind::OpenAICompletions,
            Box::new(MockProvider::new(vec![script])),
        );

        let model = Model {
            id: "mock".into(),
            name: "mock".into(),
            provider: "mock".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 8_192,
            max_tokens: 1_024,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        };

        let summarizer =
            LlmSummarizer::new(Arc::new(registry), model, Arc::new(StaticResolver), None);
        let body = "Some content the provider can't summarize for us.";
        let summary = summarizer
            .summarize(&user_message(body))
            .await
            .expect("fallback should succeed");
        // Head-truncation kicks in: short body returns as-is.
        assert_eq!(summary, body);
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
