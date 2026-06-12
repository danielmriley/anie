//! Streaming assistant-message builder used by the agent loop.
//!
//! Accumulates `ProviderEvent` deltas (text, thinking, tool calls)
//! into a single `AssistantMessage`. Extracted from `agent_loop.rs`
//! as a pure move — no logic changes.

use tracing::warn;

use anie_protocol::{AssistantMessage, ContentBlock, StopReason, ToolCall, now_millis};
use anie_provider::ProviderError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveDelta {
    Text,
    Thinking,
}

pub(crate) struct CollectedAssistant {
    pub(crate) assistant: AssistantMessage,
    pub(crate) provider_error: Option<ProviderError>,
}

pub(crate) struct AssistantMessageBuilder {
    content: Vec<BuilderContent>,
    provider: String,
    model: String,
}

impl AssistantMessageBuilder {
    pub(crate) fn new(provider: String, model: String) -> Self {
        Self {
            content: Vec::new(),
            provider,
            model,
        }
    }

    pub(crate) fn placeholder_message(&self) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: self.provider.clone(),
            model: self.model.clone(),
            timestamp: now_millis(),
            reasoning_details: None,
        }
    }

    pub(crate) fn push_text(&mut self, text: &str) {
        match self.content.last_mut() {
            Some(BuilderContent::Text(existing)) => existing.push_str(text),
            _ => self.content.push(BuilderContent::Text(text.to_string())),
        }
    }

    pub(crate) fn push_thinking(&mut self, thinking: &str) {
        match self.content.last_mut() {
            Some(BuilderContent::Thinking(existing)) => existing.push_str(thinking),
            _ => self
                .content
                .push(BuilderContent::Thinking(thinking.to_string())),
        }
    }

    pub(crate) fn start_tool_call(&mut self, tool_call: ToolCall) {
        self.content
            .push(BuilderContent::ToolCall(ToolCallBuilder::new(tool_call)));
    }

    pub(crate) fn append_tool_call_delta(&mut self, id: &str, arguments_delta: &str) {
        if let Some(tool_call) = self.find_tool_call_mut(id) {
            tool_call.arguments_buffer.push_str(arguments_delta);
        } else {
            warn!(
                tool_call_id = id,
                "received tool-call delta for unknown tool call"
            );
        }
    }

    pub(crate) fn finish_tool_call(&mut self, id: &str) {
        if let Some(tool_call) = self.find_tool_call_mut(id) {
            tool_call.finalize_arguments();
        }
    }

    pub(crate) fn finish(
        self,
        stop_reason: StopReason,
        error_message: Option<String>,
    ) -> AssistantMessage {
        let mut content: Vec<ContentBlock> = self
            .content
            .into_iter()
            .map(BuilderContent::into_content_block)
            .filter(|block| match block {
                ContentBlock::Text { text } => !text.trim().is_empty(),
                ContentBlock::Thinking { thinking, .. } => !thinking.trim().is_empty(),
                _ => true,
            })
            .collect();
        if let Some(message) = &error_message
            && content.is_empty()
        {
            content.push(ContentBlock::Text {
                text: message.clone(),
            });
        }
        AssistantMessage {
            content,
            usage: Default::default(),
            stop_reason,
            error_message,
            provider: self.provider,
            model: self.model,
            timestamp: now_millis(),
            reasoning_details: None,
        }
    }

    fn find_tool_call_mut(&mut self, id: &str) -> Option<&mut ToolCallBuilder> {
        self.content.iter_mut().find_map(|block| match block {
            BuilderContent::ToolCall(tool_call) if tool_call.id == id => Some(tool_call),
            _ => None,
        })
    }
}

enum BuilderContent {
    Text(String),
    Thinking(String),
    ToolCall(ToolCallBuilder),
}

impl BuilderContent {
    fn into_content_block(self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text { text },
            Self::Thinking(thinking) => ContentBlock::Thinking {
                thinking,
                signature: None,
            },
            Self::ToolCall(tool_call) => ContentBlock::ToolCall(tool_call.into_tool_call()),
        }
    }
}

struct ToolCallBuilder {
    id: String,
    name: String,
    arguments_value: Option<serde_json::Value>,
    arguments_buffer: String,
}

impl ToolCallBuilder {
    fn new(tool_call: ToolCall) -> Self {
        Self {
            id: tool_call.id,
            name: tool_call.name,
            arguments_value: Some(tool_call.arguments),
            arguments_buffer: String::new(),
        }
    }

    fn finalize_arguments(&mut self) {
        if self.arguments_buffer.is_empty() {
            return;
        }

        match serde_json::from_str(&self.arguments_buffer) {
            Ok(arguments) => self.arguments_value = Some(arguments),
            Err(error) => {
                self.arguments_value = Some(serde_json::json!({
                    "_raw": self.arguments_buffer,
                    "_error": error.to_string(),
                }));
            }
        }
    }

    fn into_tool_call(mut self) -> ToolCall {
        self.finalize_arguments();
        ToolCall {
            id: self.id,
            name: self.name,
            arguments: self.arguments_value.unwrap_or(serde_json::Value::Null),
        }
    }
}
