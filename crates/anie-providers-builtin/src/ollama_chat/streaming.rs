//! Ollama `/api/chat` NDJSON streaming state machine.
//!
//! # Round-trip contract
//!
//! Provider-minted fields preserved through parse -> store -> replay:
//!
//! | Field | Landing spot |
//! |-------|--------------|
//! | `message.content` | `ContentBlock::Text` |
//! | `done_reason` | `OllamaChatStreamState::done_reason` for terminal classification |
//! | `prompt_eval_count` | `Usage::input_tokens` |
//! | `eval_count` | `Usage::output_tokens` |
//!
//! Fields deferred to later PRs in this plan:
//!
//! | Field | Why safe for this PR |
//! |-------|----------------------|
//! | `message.thinking` | PR 4 adds `ThinkingDelta`; PR 3 is text-only. |
//! | `message.tool_calls` | PR 4 adds tool-call lifecycle support. |
//!
//! Intentionally dropped on replay:
//!
//! | Field | Why safe to drop |
//! |-------|------------------|
//! | `model`, `created_at`, duration timings | Telemetry/display data, not needed for replay. |
//! | image payloads | Ollama image support is explicitly deferred. |
//!
//! **Last verified against local Ollama `/api/chat`: 2026-04-25.**
//! Re-audit quarterly. Raw probe output for this PR is recorded in
//! the commit message.

use serde::Deserialize;

use anie_protocol::{AssistantMessage, ContentBlock, StopReason, Usage, now_millis};
use anie_provider::{Model, ProviderError, ProviderEvent};

use super::classify_ollama_error_body;

pub(super) struct OllamaChatStreamState {
    model: Model,
    text: String,
    usage: Usage,
    done_reason: Option<String>,
    finished: bool,
}

impl OllamaChatStreamState {
    pub(super) fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            text: String::new(),
            usage: Usage::default(),
            done_reason: None,
            finished: false,
        }
    }

    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn process_line(&mut self, line: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        let chunk: OllamaChatChunk = serde_json::from_str(line)
            .map_err(|error| ProviderError::InvalidStreamJson(error.to_string()))?;
        if let Some(error) = chunk.error {
            return Err(classify_ollama_error_body(
                reqwest::StatusCode::BAD_REQUEST,
                &error,
                None,
            ));
        }

        let mut events = Vec::new();
        if let Some(message) = chunk.message {
            if message.tool_calls.is_some() {
                return Err(ProviderError::UnsupportedStreamFeature(
                    "Ollama tool_calls are implemented in the native tool-call PR".into(),
                ));
            }
            if let Some(content) = message.content
                && !content.is_empty()
            {
                self.text.push_str(&content);
                events.push(ProviderEvent::TextDelta(content));
            }
        }

        if chunk.done {
            if let Some(done_reason) = chunk.done_reason {
                self.done_reason = Some(done_reason);
            }
            if let Some(input_tokens) = chunk.prompt_eval_count {
                self.usage.input_tokens = input_tokens;
            }
            if let Some(output_tokens) = chunk.eval_count {
                self.usage.output_tokens = output_tokens;
            }
            if self.usage.input_tokens > 0 || self.usage.output_tokens > 0 {
                self.usage.total_tokens = Some(self.usage.input_tokens + self.usage.output_tokens);
            }
            events.extend(self.finish_stream()?);
        }

        Ok(events)
    }

    pub(super) fn finish_stream(&mut self) -> Result<Vec<ProviderEvent>, ProviderError> {
        if !self.has_meaningful_content() {
            if self.done_reason.as_deref() == Some("length") {
                return Err(ProviderError::ResponseTruncated);
            }
            return Err(ProviderError::EmptyAssistantResponse);
        }

        Ok(vec![ProviderEvent::Done(self.into_message())])
    }

    fn has_meaningful_content(&self) -> bool {
        !self.text.is_empty()
    }

    #[allow(clippy::wrong_self_convention)]
    fn into_message(&mut self) -> AssistantMessage {
        self.finished = true;
        AssistantMessage {
            content: vec![ContentBlock::Text {
                text: std::mem::take(&mut self.text),
            }],
            usage: std::mem::take(&mut self.usage),
            stop_reason: match self.done_reason.as_deref() {
                Some("stop") | Some("length") | None => StopReason::Stop,
                _ => StopReason::Stop,
            },
            error_message: None,
            provider: self.model.provider.clone(),
            model: self.model.id.clone(),
            timestamp: now_millis(),
            reasoning_details: None,
        }
    }
}

#[derive(Deserialize)]
struct OllamaChatChunk {
    #[serde(default)]
    message: Option<OllamaChatMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct OllamaChatMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<serde_json::Value>>,
}

#[cfg(test)]
mod tests {
    use anie_provider::{ApiKind, CostPerMillion, ModelCompat};

    use super::*;

    fn sample_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OllamaChatApi,
            base_url: "http://localhost:11434".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn final_text(events: &[ProviderEvent]) -> String {
        let Some(ProviderEvent::Done(message)) = events.last() else {
            panic!("expected done event");
        };
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>()
    }

    #[test]
    fn streaming_state_emits_text_deltas_then_done() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let first = state
            .process_line(r#"{"message":{"role":"assistant","content":"hi"},"done":false}"#)
            .expect("first line");
        assert_eq!(first, vec![ProviderEvent::TextDelta("hi".into())]);

        let done = state
            .process_line(
                r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":2}"#,
            )
            .expect("done line");

        assert_eq!(final_text(&done), "hi");
        let Some(ProviderEvent::Done(message)) = done.last() else {
            panic!("expected done");
        };
        assert_eq!(message.usage.input_tokens, 3);
        assert_eq!(message.usage.output_tokens, 2);
        assert_eq!(message.usage.total_tokens, Some(5));
    }

    #[test]
    fn streaming_state_routes_done_reason_length_to_response_truncated() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let error = state
            .process_line(r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"length"}"#)
            .expect_err("length with no visible content should truncate");

        assert_eq!(error, ProviderError::ResponseTruncated);
    }

    #[test]
    fn streaming_state_routes_inline_error_to_provider_error() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let error = state
            .process_line(r#"{"error":"model 'nope:1b' not found"}"#)
            .expect_err("inline error");

        assert!(matches!(error, ProviderError::Http { status: 400, .. }));
    }

    #[test]
    fn empty_assistant_response_surfaces_as_typed_error() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let error = state
            .process_line(
                r#"{"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop"}"#,
            )
            .expect_err("empty response");

        assert_eq!(error, ProviderError::EmptyAssistantResponse);
    }
}
