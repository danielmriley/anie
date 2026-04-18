use std::{collections::VecDeque, sync::Mutex};

use async_stream::stream;

use anie_protocol::{AssistantMessage, Message, ToolDef};

use crate::{
    LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderEvent, ProviderStream,
    StreamOptions,
};

/// One scripted provider response returned by the mock provider.
#[derive(Debug)]
pub struct MockStreamScript {
    items: Vec<Result<ProviderEvent, ProviderError>>,
}

impl MockStreamScript {
    /// Build a scripted stream from raw provider events.
    #[must_use]
    pub fn new(items: Vec<Result<ProviderEvent, ProviderError>>) -> Self {
        Self { items }
    }

    /// Build a trivial stream that ends with a final assistant message.
    #[must_use]
    pub fn from_message(message: AssistantMessage) -> Self {
        Self {
            items: vec![Ok(ProviderEvent::Done(message))],
        }
    }

    /// Build a stream that fails immediately with a structured provider error.
    #[must_use]
    pub fn from_error(error: ProviderError) -> Self {
        Self {
            items: vec![Err(error)],
        }
    }
}

/// A feature-gated provider used to unit test the agent loop.
pub struct MockProvider {
    scripts: Mutex<VecDeque<MockStreamScript>>,
}

impl MockProvider {
    /// Create a mock provider with a queue of scripted responses.
    #[must_use]
    pub fn new(scripts: Vec<MockStreamScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
        }
    }
}

impl Provider for MockProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: LlmContext,
        _options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let script = self
            .scripts
            .lock()
            .map_err(|_| ProviderError::Other("mock provider mutex poisoned".into()))?
            .pop_front()
            .ok_or_else(|| ProviderError::Request("no scripted mock response available".into()))?;

        let event_stream = stream! {
            for item in script.items {
                yield item;
            }
        };

        Ok(Box::pin(event_stream))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        messages
            .iter()
            .map(|message| LlmMessage {
                role: match message {
                    Message::User(_) => "user",
                    Message::Assistant(_) => "assistant",
                    Message::ToolResult(_) => "tool",
                    Message::Custom(_) => "custom",
                }
                .to_string(),
                content: serde_json::to_value(message).unwrap_or(serde_json::Value::Null),
            })
            .collect()
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect()
    }
}
