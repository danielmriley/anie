//! Ollama `/api/chat` NDJSON streaming state machine.
//!
//! # Round-trip contract
//!
//! Provider-minted fields preserved through parse -> store -> replay:
//!
//! | Field | Landing spot |
//! |-------|--------------|
//! | `message.content` | `ContentBlock::Text` |
//! | `message.thinking` | `ContentBlock::Thinking` with no signature |
//! | `message.tool_calls[].function.name` | `ContentBlock::ToolCall.name` |
//! | `message.tool_calls[].function.arguments` | `ContentBlock::ToolCall.arguments` |
//! | `done_reason` | `OllamaChatStreamState::done_reason` for terminal classification |
//! | `prompt_eval_count` | `Usage::input_tokens` |
//! | `eval_count` | `Usage::output_tokens` |
//!
//! Synthesized by anie:
//!
//! | Field | Why synthesized |
//! |-------|-----------------|
//! | `ToolCall.id` | Ollama does not emit a stable tool-call id. |
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

use anie_protocol::{AssistantMessage, ContentBlock, StopReason, ToolCall, Usage, now_millis};
use anie_provider::{Model, ProviderError, ProviderEvent};

use super::classify_ollama_error_body;

pub(super) struct OllamaChatStreamState {
    model: Model,
    thinking: String,
    text: String,
    tool_calls: Vec<ToolCall>,
    tool_call_counter: u64,
    usage: Usage,
    done_reason: Option<String>,
    finished: bool,
}

impl OllamaChatStreamState {
    pub(super) fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            thinking: String::new(),
            text: String::new(),
            tool_calls: Vec::new(),
            tool_call_counter: 0,
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
            if let Some(thinking) = message.thinking
                && !thinking.is_empty()
            {
                self.thinking.push_str(&thinking);
                events.push(ProviderEvent::ThinkingDelta(thinking));
            }
            if let Some(content) = message.content
                && !content.is_empty()
            {
                self.text.push_str(&content);
                events.push(ProviderEvent::TextDelta(content));
            }
            if let Some(tool_calls) = message.tool_calls {
                for tool_call in tool_calls {
                    events.extend(self.push_tool_call(tool_call)?);
                }
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
        !self.text.is_empty() || !self.tool_calls.is_empty()
    }

    fn push_tool_call(
        &mut self,
        tool_call: OllamaToolCall,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.tool_call_counter += 1;
        let id = format!("ollama_tool_call_{}", self.tool_call_counter);
        let arguments = tool_call
            .function
            .arguments
            .unwrap_or(serde_json::Value::Null);
        let arguments_delta = serde_json::to_string(&arguments).map_err(|error| {
            ProviderError::ToolCallMalformed(format!(
                "failed to serialize Ollama tool-call arguments: {error}"
            ))
        })?;
        let call = ToolCall {
            id: id.clone(),
            name: tool_call.function.name,
            arguments,
        };
        let start = ToolCall {
            id: id.clone(),
            name: call.name.clone(),
            arguments: serde_json::Value::Null,
        };
        self.tool_calls.push(call);
        Ok(vec![
            ProviderEvent::ToolCallStart(start),
            ProviderEvent::ToolCallDelta {
                id: id.clone(),
                arguments_delta,
            },
            ProviderEvent::ToolCallEnd { id },
        ])
    }

    #[allow(clippy::wrong_self_convention)]
    fn into_message(&mut self) -> AssistantMessage {
        self.finished = true;
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: std::mem::take(&mut self.thinking),
                signature: None,
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: std::mem::take(&mut self.text),
            });
        }
        for tool_call in std::mem::take(&mut self.tool_calls) {
            content.push(ContentBlock::ToolCall(tool_call));
        }
        let used_tool = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolCall(_)));
        AssistantMessage {
            content,
            usage: std::mem::take(&mut self.usage),
            stop_reason: match self.done_reason.as_deref() {
                Some("tool_calls") => StopReason::ToolUse,
                _ if used_tool => StopReason::ToolUse,
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
    thinking: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Deserialize)]
struct OllamaToolCall {
    function: OllamaToolFunction,
}

#[derive(Deserialize)]
struct OllamaToolFunction {
    name: String,
    #[serde(default)]
    arguments: Option<serde_json::Value>,
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

    #[test]
    fn streaming_state_emits_thinking_deltas_when_think_is_true() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let events = state
            .process_line(r#"{"message":{"role":"assistant","thinking":"plan"},"done":false}"#)
            .expect("thinking line");

        assert_eq!(events, vec![ProviderEvent::ThinkingDelta("plan".into())]);
    }

    #[test]
    fn streaming_state_emits_tool_call_lifecycle_for_arguments_object() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let events = state
            .process_line(
                r#"{"message":{"role":"assistant","tool_calls":[{"function":{"name":"read_file","arguments":{"path":"Cargo.toml"}}}]},"done":false}"#,
            )
            .expect("tool call line");

        assert!(matches!(
            events.first(),
            Some(ProviderEvent::ToolCallStart(ToolCall { name, .. })) if name == "read_file"
        ));
        assert!(matches!(
            events.get(1),
            Some(ProviderEvent::ToolCallDelta { arguments_delta, .. })
                if arguments_delta == r#"{"path":"Cargo.toml"}"#
        ));
        assert!(matches!(
            events.get(2),
            Some(ProviderEvent::ToolCallEnd { .. })
        ));
    }

    #[test]
    fn streaming_state_populates_usage_from_done_line() {
        let mut state = OllamaChatStreamState::new(&sample_model());
        state
            .process_line(r#"{"message":{"role":"assistant","content":"ok"},"done":false}"#)
            .expect("text line");

        let events = state
            .process_line(
                r#"{"done":true,"done_reason":"stop","prompt_eval_count":13,"eval_count":21}"#,
            )
            .expect("done line");

        let Some(ProviderEvent::Done(message)) = events.last() else {
            panic!("expected done");
        };
        assert_eq!(message.usage.input_tokens, 13);
        assert_eq!(message.usage.output_tokens, 21);
        assert_eq!(message.usage.total_tokens, Some(34));
    }

    #[test]
    fn tool_call_id_is_synthesized_when_ollama_omits_it() {
        let mut state = OllamaChatStreamState::new(&sample_model());

        let events = state
            .process_line(
                r#"{"message":{"role":"assistant","tool_calls":[{"function":{"name":"read_file","arguments":{}}}]},"done":false}"#,
            )
            .expect("tool call line");

        let Some(ProviderEvent::ToolCallStart(tool_call)) = events.first() else {
            panic!("expected tool call start");
        };
        assert_eq!(tool_call.id, "ollama_tool_call_1");
    }
}
