//! OpenAI-compatible streaming reassembly state machine.
//!
//! `OpenAiStreamState` consumes SSE `data:` payloads from an OpenAI chat-
//! completions stream and emits `ProviderEvent`s: text deltas, thinking
//! deltas (both native `reasoning` field and tagged `<think>…</think>`
//! content), tool-call lifecycle events, and a terminal `Done` event with
//! the accumulated `AssistantMessage`.
//!
//! The state machine delegates tag extraction to `TaggedReasoningSplitter`
//! (see `super::tagged_reasoning`) and per-slot tool-call tracking to
//! `OpenAiToolCallState` (local to this module).
//!
//! # Round-trip contract
//!
//! Provider-minted opaque fields preserved through parse → store →
//! replay:
//!
//! | Field                         | Landing spot                             |
//! |-------------------------------|------------------------------------------|
//! | `tool_calls[].id`             | `ToolCall::id`                           |
//! | `tool_calls[].function.name`  | `ToolCall::name`                         |
//! | `tool_calls[].function.args`  | `ToolCall::arguments` (accumulated)      |
//!
//! Intentionally dropped on replay (captured for display only):
//!
//! | Field                                 | Why                           |
//! |---------------------------------------|-------------------------------|
//! | `reasoning` / `reasoning_content` /   | OpenAI chat-completions does  |
//! | `thinking` deltas                     | not round-trip reasoning as   |
//! |                                       | assistant content.            |
//! | Tagged `<think>…</think>` etc.        | Local-model thinking output;  |
//! |                                       | same reason.                  |
//!
//! **Last verified against provider docs: 2026-04-19.** Re-audit
//! quarterly. See docs/api_integrity_plans/03a_stream_field_audit.md.

use std::collections::BTreeMap;

use serde_json::json;

use anie_protocol::{AssistantMessage, ContentBlock, StopReason, ToolCall, Usage, now_millis};
use anie_provider::{Model, ProviderError, ProviderEvent};

use super::reasoning_strategy::native_reasoning_delta;
use super::tagged_reasoning::{StreamContentPart, TaggedReasoningSplitter};

pub(super) struct OpenAiStreamState {
    model: Model,
    text: String,
    thinking: String,
    tagged_reasoning: TaggedReasoningSplitter,
    tool_calls: BTreeMap<usize, OpenAiToolCallState>,
    usage: Usage,
    finish_reason: Option<String>,
    finished: bool,
}

impl OpenAiStreamState {
    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            text: String::new(),
            thinking: String::new(),
            tagged_reasoning: TaggedReasoningSplitter::default(),
            tool_calls: BTreeMap::new(),
            usage: Usage::default(),
            finish_reason: None,
            finished: false,
        }
    }

    pub(super) fn process_event(
        &mut self,
        data: &str,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        if data == "[DONE]" {
            return self.finish_stream();
        }

        let payload: serde_json::Value = serde_json::from_str(data)
            .map_err(|error| ProviderError::InvalidStreamJson(error.to_string()))?;
        let mut events = Vec::new();

        if let Some(usage) = payload.get("usage") {
            self.usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
            self.usage.total_tokens = usage["total_tokens"].as_u64();
        }

        if let Some(choices) = payload.get("choices").and_then(serde_json::Value::as_array) {
            for choice in choices {
                let Some(delta) = choice.get("delta") else {
                    if let Some(finish_reason) = choice
                        .get("finish_reason")
                        .and_then(serde_json::Value::as_str)
                        && !finish_reason.is_empty()
                    {
                        self.finish_reason = Some(finish_reason.to_string());
                        if finish_reason == "tool_calls" {
                            events.extend(self.finish_tool_calls());
                        }
                    }
                    continue;
                };

                let has_native_reasoning = if let Some(reasoning) = native_reasoning_delta(delta) {
                    self.thinking.push_str(&reasoning);
                    events.push(ProviderEvent::ThinkingDelta(reasoning));
                    true
                } else {
                    false
                };

                if let Some(content) = delta.get("content").and_then(serde_json::Value::as_str) {
                    if has_native_reasoning {
                        if !content.is_empty() {
                            self.text.push_str(content);
                            events.push(ProviderEvent::TextDelta(content.to_string()));
                        }
                    } else {
                        let parts = self.tagged_reasoning.push(content);
                        self.push_stream_content_parts(parts, &mut events);
                    }
                }

                if let Some(tool_calls) = delta
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                {
                    for tool_call in tool_calls {
                        let index = tool_call
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0) as usize;
                        let state = self.tool_calls.entry(index).or_default();
                        if let Some(id) = tool_call.get("id").and_then(serde_json::Value::as_str) {
                            state.id = id.to_string();
                        }
                        if let Some(name) = tool_call
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(serde_json::Value::as_str)
                        {
                            state.name = name.to_string();
                        }
                        if !state.started && !state.id.is_empty() && !state.name.is_empty() {
                            state.started = true;
                            events.push(ProviderEvent::ToolCallStart(ToolCall {
                                id: state.id.clone(),
                                name: state.name.clone(),
                                arguments: serde_json::Value::Null,
                            }));
                        }
                        if let Some(arguments_delta) = tool_call
                            .get("function")
                            .and_then(|function| function.get("arguments"))
                            .and_then(serde_json::Value::as_str)
                            && !arguments_delta.is_empty()
                        {
                            state.arguments.push_str(arguments_delta);
                            events.push(ProviderEvent::ToolCallDelta {
                                id: state.id.clone(),
                                arguments_delta: arguments_delta.to_string(),
                            });
                        }
                    }
                }

                if let Some(finish_reason) = choice
                    .get("finish_reason")
                    .and_then(serde_json::Value::as_str)
                    && !finish_reason.is_empty()
                {
                    self.finish_reason = Some(finish_reason.to_string());
                    if finish_reason == "tool_calls" {
                        events.extend(self.finish_tool_calls());
                    }
                }
            }
        }

        Ok(events)
    }

    fn push_stream_content_parts(
        &mut self,
        parts: Vec<StreamContentPart>,
        events: &mut Vec<ProviderEvent>,
    ) {
        for part in parts {
            match part {
                StreamContentPart::Text(text) => {
                    self.text.push_str(&text);
                    events.push(ProviderEvent::TextDelta(text));
                }
                StreamContentPart::Thinking(thinking) => {
                    self.thinking.push_str(&thinking);
                    events.push(ProviderEvent::ThinkingDelta(thinking));
                }
            }
        }
    }

    fn finish_tagged_content(&mut self) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let parts = self.tagged_reasoning.finish();
        self.push_stream_content_parts(parts, &mut events);
        events
    }

    pub(super) fn finish_stream(&mut self) -> Result<Vec<ProviderEvent>, ProviderError> {
        let mut events = self.finish_tagged_content();
        events.extend(self.finish_tool_calls());
        if !self.has_meaningful_content() {
            return Err(ProviderError::EmptyAssistantResponse);
        }
        events.push(ProviderEvent::Done(self.into_message()));
        Ok(events)
    }

    fn finish_tool_calls(&mut self) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        for state in self.tool_calls.values_mut() {
            if !state.ended && !state.id.is_empty() {
                state.ended = true;
                events.push(ProviderEvent::ToolCallEnd {
                    id: state.id.clone(),
                });
            }
        }
        events
    }

    fn has_meaningful_content(&self) -> bool {
        !self.text.is_empty() || self.tool_calls.values().any(|state| !state.id.is_empty())
    }

    // Materializes the accumulated state into an `AssistantMessage`. Uses
    // `&mut self` (not `self`) so the state machine can keep draining any
    // buffered tagged-reasoning content before returning.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn into_message(&mut self) -> AssistantMessage {
        self.finished = true;
        let _ = self.finish_tagged_content();
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
        for state in self.tool_calls.values_mut() {
            let arguments = match serde_json::from_str(&state.arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    json!({
                        "_raw": state.arguments,
                        "_error": error.to_string(),
                    })
                }
            };
            content.push(ContentBlock::ToolCall(ToolCall {
                id: state.id.clone(),
                name: state.name.clone(),
                arguments,
            }));
        }

        AssistantMessage {
            content,
            usage: std::mem::take(&mut self.usage),
            stop_reason: match self.finish_reason.as_deref() {
                Some("tool_calls") => StopReason::ToolUse,
                Some("stop") | Some("length") | None => StopReason::Stop,
                _ => StopReason::Stop,
            },
            error_message: None,
            provider: self.model.provider.clone(),
            model: self.model.id.clone(),
            timestamp: now_millis(),
        }
    }
}

#[derive(Default)]
struct OpenAiToolCallState {
    id: String,
    name: String,
    arguments: String,
    started: bool,
    ended: bool,
}

#[cfg(test)]
mod tests {
    use anie_protocol::{AssistantMessage, ContentBlock, ToolCall};
    use anie_provider::{ApiKind, CostPerMillion, Model, ProviderError, ProviderEvent};

    use super::*;

    fn sample_model() -> Model {
        Model {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".into(),
            context_window: 128_000,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: true,
            cost_per_million: CostPerMillion::zero(),
        }
    }

    fn sample_local_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost:11434/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
        }
    }

    fn assistant_text(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn assistant_thinking(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn final_message(events: &[ProviderEvent]) -> AssistantMessage {
        events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::Done(message) => Some(message.clone()),
                _ => None,
            })
            .expect("done event")
    }

    #[test]
    fn accumulates_argument_fragments_into_tool_calls() {
        let mut state = OpenAiStreamState::new(&sample_model());
        let first = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#)
            .expect("first chunk");
        let second = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"src/main.rs\"}"}}],"content":""},"finish_reason":"tool_calls"}]}"#)
            .expect("second chunk");

        assert!(matches!(
            first.first(),
            Some(ProviderEvent::ToolCallStart(_))
        ));
        assert!(matches!(
            second.last(),
            Some(ProviderEvent::ToolCallEnd { .. })
        ));

        let message = state.into_message();
        assert!(matches!(
            &message.content[0],
            ContentBlock::ToolCall(ToolCall { arguments, .. }) if arguments == &json!({ "path": "src/main.rs" })
        ));
    }

    #[test]
    fn handles_missing_usage_fields() {
        let mut state = OpenAiStreamState::new(&sample_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        assert!(matches!(events.first(), Some(ProviderEvent::TextDelta(text)) if text == "hello"));
        let message = state.into_message();
        assert_eq!(message.usage.input_tokens, 0);
        assert_eq!(message.usage.output_tokens, 0);
    }

    #[test]
    fn parses_native_reasoning_fields_into_thinking_deltas() {
        for (field, value) in [
            ("reasoning", "plan"),
            ("reasoning_content", "legacy-plan"),
            ("thinking", "older-plan"),
        ] {
            let mut state = OpenAiStreamState::new(&sample_local_model());
            let events = state
                .process_event(
                    &json!({
                        "choices": [{
                            "index": 0,
                            "delta": { field: value },
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string(),
                )
                .expect("events");
            let message = state.into_message();

            assert_eq!(events, vec![ProviderEvent::ThinkingDelta(value.into())]);
            assert_eq!(assistant_thinking(&message), value);
            assert_eq!(assistant_text(&message), "");
        }
    }

    #[test]
    fn reasoning_only_stream_without_visible_content_is_an_error() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"","reasoning":"hello from reasoning"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let error = state
            .finish_stream()
            .expect_err("finish stream should fail");

        assert_eq!(
            events,
            vec![ProviderEvent::ThinkingDelta("hello from reasoning".into())]
        );
        assert!(matches!(error, ProviderError::EmptyAssistantResponse));
    }

    #[test]
    fn reasoning_with_visible_text_still_succeeds() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"reasoning":"plan","content":"answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");

        let finished = state.finish_stream().expect("finish stream");
        let message = final_message(&finished);

        assert_eq!(assistant_thinking(&message), "plan");
        assert_eq!(assistant_text(&message), "answer");
    }

    #[test]
    fn same_event_native_reasoning_and_text_are_both_preserved() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"reasoning":"plan","content":"answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let message = state.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("plan".into()),
                ProviderEvent::TextDelta("answer".into()),
            ]
        );
        assert_eq!(assistant_thinking(&message), "plan");
        assert_eq!(assistant_text(&message), "answer");
    }

    #[test]
    fn tagged_parsing_remains_a_fallback_when_native_reasoning_fields_are_absent() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"<think>plan</think>answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let message = state.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("plan".into()),
                ProviderEvent::TextDelta("answer".into()),
            ]
        );
        assert_eq!(assistant_thinking(&message), "plan");
        assert_eq!(assistant_text(&message), "answer");
    }

    #[test]
    fn parses_tagged_reasoning_when_opening_tag_is_split_across_chunks() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let first = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"content":"Before<thi"}}]}"#)
            .expect("first chunk");
        let second = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"nk>plan</think> after"},"finish_reason":"stop"}]}"#,
            )
            .expect("second chunk");
        let finished = state.finish_stream().expect("finish stream");
        let message = final_message(&finished);

        assert_eq!(first, vec![ProviderEvent::TextDelta("Before".into())]);
        assert_eq!(
            second,
            vec![
                ProviderEvent::ThinkingDelta("plan".into()),
                ProviderEvent::TextDelta(" after".into()),
            ]
        );
        assert_eq!(assistant_text(&message), "Before after");
        assert_eq!(assistant_thinking(&message), "plan");
        assert!(!assistant_text(&message).contains("<think>"));
    }

    #[test]
    fn parses_tagged_reasoning_when_closing_tag_is_split_across_chunks() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let first = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"content":"<think>plan</thi"}}]}"#)
            .expect("first chunk");
        let second = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"nk> answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("second chunk");
        let message = state.into_message();

        assert_eq!(first, vec![ProviderEvent::ThinkingDelta("plan".into())]);
        assert_eq!(second, vec![ProviderEvent::TextDelta(" answer".into())]);
        assert_eq!(assistant_text(&message), " answer");
        assert_eq!(assistant_thinking(&message), "plan");
    }

    #[test]
    fn parses_multiple_tagged_reasoning_spans_in_one_response() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"A<think>X</think>B<reasoning>Y</reasoning>C"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let message = state.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("A".into()),
                ProviderEvent::ThinkingDelta("X".into()),
                ProviderEvent::TextDelta("B".into()),
                ProviderEvent::ThinkingDelta("Y".into()),
                ProviderEvent::TextDelta("C".into()),
            ]
        );
        assert_eq!(assistant_text(&message), "ABC");
        assert_eq!(assistant_thinking(&message), "XY");
    }

    #[test]
    fn tagged_reasoning_aliases_all_emit_thinking() {
        for (content, expected) in [
            ("<think>alpha</think>", "alpha"),
            ("<thinking>beta</thinking>", "beta"),
            ("<reasoning>gamma</reasoning>", "gamma"),
        ] {
            let mut state = OpenAiStreamState::new(&sample_local_model());
            let events = state
                .process_event(
                    &json!({
                        "choices": [{
                            "index": 0,
                            "delta": { "content": content },
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string(),
                )
                .expect("events");
            let message = state.into_message();

            assert_eq!(events, vec![ProviderEvent::ThinkingDelta(expected.into())]);
            assert_eq!(assistant_text(&message), "");
            assert_eq!(assistant_thinking(&message), expected);
        }
    }

    #[test]
    fn malformed_or_unclosed_tag_sequences_do_not_lose_content() {
        let mut malformed = OpenAiStreamState::new(&sample_local_model());
        let first = malformed
            .process_event(r#"{"choices":[{"index":0,"delta":{"content":"hello <thi"}}]}"#)
            .expect("malformed first chunk");
        let finished = malformed.finish_stream().expect("finish stream");
        let malformed_message = final_message(&finished);

        assert_eq!(first, vec![ProviderEvent::TextDelta("hello ".into())]);
        assert!(matches!(
            finished.first(),
            Some(ProviderEvent::TextDelta(text)) if text == "<thi"
        ));
        assert_eq!(assistant_text(&malformed_message), "hello <thi");
        assert_eq!(assistant_thinking(&malformed_message), "");

        let mut unclosed = OpenAiStreamState::new(&sample_local_model());
        let events = unclosed
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"start<think>plan"},"finish_reason":"stop"}]}"#,
            )
            .expect("unclosed events");
        let unclosed_message = unclosed.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("start".into()),
                ProviderEvent::ThinkingDelta("plan".into()),
            ]
        );
        assert_eq!(assistant_text(&unclosed_message), "start");
        assert_eq!(assistant_thinking(&unclosed_message), "plan");
    }

    #[test]
    fn truly_empty_successful_stop_becomes_stream_error() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let error = state
            .finish_stream()
            .expect_err("empty response should error");

        assert!(events.is_empty());
        assert!(matches!(error, ProviderError::EmptyAssistantResponse));
    }
}
