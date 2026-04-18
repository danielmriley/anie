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

        let payload: serde_json::Value =
            serde_json::from_str(data).map_err(|error| ProviderError::Stream(error.to_string()))?;
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
            return Err(ProviderError::Stream("empty assistant response".into()));
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
