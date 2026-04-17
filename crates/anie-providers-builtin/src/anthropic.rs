use std::collections::BTreeMap;

use async_stream::try_stream;
use futures::StreamExt;
use serde_json::json;

use anie_protocol::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolDef, ToolResultMessage,
    Usage, now_millis,
};
use anie_provider::{
    LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderEvent, ProviderStream,
    StreamOptions, ThinkingLevel,
};

use crate::{classify_http_error, create_http_client, parse_retry_after, sse_stream};

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: create_http_client(),
        }
    }

    fn build_request_body(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(model.id));
        body.insert(
            "max_tokens".into(),
            json!(options.max_tokens.unwrap_or(model.max_tokens)),
        );
        body.insert("stream".into(), serde_json::Value::Bool(true));
        body.insert(
            "messages".into(),
            serde_json::Value::Array(
                context
                    .messages
                    .iter()
                    .map(|message| {
                        json!({
                            "role": message.role,
                            "content": message.content,
                        })
                    })
                    .collect(),
            ),
        );
        if !context.system_prompt.is_empty() {
            body.insert(
                "system".into(),
                serde_json::Value::Array(vec![json!({
                    "type": "text",
                    "text": context.system_prompt,
                    "cache_control": { "type": "ephemeral" },
                })]),
            );
        }
        let tools = self.convert_tools(&context.tools);
        if !tools.is_empty() {
            body.insert("tools".into(), serde_json::Value::Array(tools));
        }
        if let Some(temperature) = options.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(thinking) = thinking_config(
            options.thinking,
            options.max_tokens.unwrap_or(model.max_tokens),
        ) {
            body.insert("thinking".into(), thinking);
            body.insert("temperature".into(), json!(1.0));
        }
        serde_json::Value::Object(body)
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for AnthropicProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let client = self.client.clone();
        let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));
        let body = self.build_request_body(model, &context, &options);
        let model_clone = model.clone();
        let thinking_enabled = options.thinking != ThinkingLevel::Off;

        let stream = try_stream! {
            let mut request = client
                .post(url)
                .header("anthropic-version", "2023-06-01")
                .header(reqwest::header::CONTENT_TYPE, "application/json");
            if let Some(api_key) = &options.api_key {
                request = request.header("x-api-key", api_key);
            }
            if thinking_enabled {
                request = request.header("anthropic-beta", "interleaved-thinking-2025-05-14");
            }
            for (name, value) in &options.headers {
                request = request.header(name, value);
            }

            let response = request
                .json(&body)
                .send()
                .await
                .map_err(|error| ProviderError::Request(error.to_string()))?;
            let response = if response.status().is_success() {
                response
            } else {
                let status = response.status();
                let retry_after = parse_retry_after(&response);
                let body = response.text().await.unwrap_or_default();
                Err(classify_http_error(status, &body, retry_after))?
            };

            let mut events = sse_stream(response);
            let mut state = AnthropicStreamState::new(model_clone);
            while let Some(event) = events.next().await {
                let event = event.map_err(|error| ProviderError::Stream(error.to_string()))?;
                for provider_event in state.process_event(&event.event_type, &event.data)? {
                    yield provider_event;
                }
            }

            if !state.finished {
                yield ProviderEvent::Done(state.into_message());
            }
        };

        Ok(Box::pin(stream))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        let mut converted = Vec::new();
        let mut pending_tool_results = Vec::new();

        for message in messages {
            match message {
                Message::ToolResult(tool_result) => {
                    pending_tool_results.push(tool_result_to_anthropic_content(tool_result));
                }
                _ => {
                    if !pending_tool_results.is_empty() {
                        converted.push(LlmMessage {
                            role: "user".into(),
                            content: serde_json::Value::Array(std::mem::take(
                                &mut pending_tool_results,
                            )),
                        });
                    }
                    converted.push(convert_single_message(message));
                }
            }
        }

        if !pending_tool_results.is_empty() {
            converted.push(LlmMessage {
                role: "user".into(),
                content: serde_json::Value::Array(pending_tool_results),
            });
        }

        converted
    }

    fn includes_thinking_in_replay(&self) -> bool {
        true
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                    "cache_control": { "type": "ephemeral" },
                })
            })
            .collect()
    }
}

fn convert_single_message(message: &Message) -> LlmMessage {
    match message {
        Message::User(user_message) => LlmMessage {
            role: "user".into(),
            content: serde_json::Value::Array(content_blocks_to_anthropic(&user_message.content)),
        },
        Message::Assistant(assistant_message) => LlmMessage {
            role: "assistant".into(),
            content: serde_json::Value::Array(content_blocks_to_anthropic(
                &assistant_message.content,
            )),
        },
        Message::ToolResult(_) => unreachable!("tool results are batched before conversion"),
        Message::Custom(custom_message) => LlmMessage {
            role: "user".into(),
            content: serde_json::Value::Array(vec![json!({
                "type": "text",
                "text": format!(
                    "[custom:{}]\n{}",
                    custom_message.custom_type,
                    serde_json::to_string(&custom_message.content).unwrap_or_default(),
                ),
            })]),
        },
    }
}

fn content_blocks_to_anthropic(content: &[ContentBlock]) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            })),
            ContentBlock::Thinking { thinking } => {
                Some(json!({ "type": "thinking", "thinking": thinking }))
            }
            ContentBlock::ToolCall(tool_call) => Some(json!({
                "type": "tool_use",
                "id": tool_call.id,
                "name": tool_call.name,
                "input": tool_call.arguments,
            })),
        })
        .collect()
}

fn tool_result_to_anthropic_content(tool_result: &ToolResultMessage) -> serde_json::Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_result.tool_call_id,
        "content": anthropic_tool_result_content(&tool_result.content),
        "is_error": tool_result.is_error,
    })
}

fn anthropic_tool_result_content(content: &[ContentBlock]) -> serde_json::Value {
    if let [ContentBlock::Text { text }] = content {
        return serde_json::Value::String(text.clone());
    }
    serde_json::Value::Array(content_blocks_to_anthropic(content))
}

fn thinking_config(thinking: ThinkingLevel, max_tokens: u64) -> Option<serde_json::Value> {
    match thinking {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some(json!({
            "type": "enabled",
            "budget_tokens": (max_tokens / 4).max(1_024),
        })),
        ThinkingLevel::Medium => Some(json!({
            "type": "enabled",
            "budget_tokens": (max_tokens / 2).max(2_048),
        })),
        ThinkingLevel::High => Some(json!({
            "type": "enabled",
            "budget_tokens": max_tokens.max(4_096),
        })),
    }
}

struct AnthropicStreamState {
    model: Model,
    usage: Usage,
    blocks: BTreeMap<usize, AnthropicBlockState>,
    stop_reason: StopReason,
    finished: bool,
}

impl AnthropicStreamState {
    fn new(model: Model) -> Self {
        Self {
            model,
            usage: Usage::default(),
            blocks: BTreeMap::new(),
            stop_reason: StopReason::Stop,
            finished: false,
        }
    }

    fn process_event(
        &mut self,
        event_type: &str,
        data: &str,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        let payload: serde_json::Value =
            serde_json::from_str(data).map_err(|error| ProviderError::Stream(error.to_string()))?;
        let mut events = Vec::new();

        match event_type {
            "message_start" => {
                update_usage(&mut self.usage, &payload["message"]["usage"]);
                events.push(ProviderEvent::Start);
            }
            "content_block_start" => {
                let index = payload["index"].as_u64().unwrap_or(0) as usize;
                let block = &payload["content_block"];
                match block["type"].as_str() {
                    Some("text") => {
                        self.blocks
                            .insert(index, AnthropicBlockState::Text(String::new()));
                    }
                    Some("thinking") => {
                        self.blocks
                            .insert(index, AnthropicBlockState::Thinking(String::new()));
                    }
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap_or_default().to_string();
                        let name = block["name"].as_str().unwrap_or_default().to_string();
                        let input = block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        self.blocks.insert(
                            index,
                            AnthropicBlockState::ToolUse(AnthropicToolUseState {
                                id: id.clone(),
                                name: name.clone(),
                                input,
                                partial_json: String::new(),
                            }),
                        );
                        events.push(ProviderEvent::ToolCallStart(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Null,
                        }));
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = payload["index"].as_u64().unwrap_or(0) as usize;
                let delta = &payload["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default().to_string();
                        if let Some(AnthropicBlockState::Text(existing)) =
                            self.blocks.get_mut(&index)
                        {
                            existing.push_str(&text);
                        }
                        events.push(ProviderEvent::TextDelta(text));
                    }
                    Some("thinking_delta") => {
                        let thinking = delta
                            .get("thinking")
                            .and_then(serde_json::Value::as_str)
                            .or_else(|| delta.get("text").and_then(serde_json::Value::as_str))
                            .unwrap_or_default()
                            .to_string();
                        if let Some(AnthropicBlockState::Thinking(existing)) =
                            self.blocks.get_mut(&index)
                        {
                            existing.push_str(&thinking);
                        }
                        events.push(ProviderEvent::ThinkingDelta(thinking));
                    }
                    Some("input_json_delta") => {
                        let partial_json = delta["partial_json"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        if let Some(AnthropicBlockState::ToolUse(tool_use)) =
                            self.blocks.get_mut(&index)
                        {
                            tool_use.partial_json.push_str(&partial_json);
                            events.push(ProviderEvent::ToolCallDelta {
                                id: tool_use.id.clone(),
                                arguments_delta: partial_json,
                            });
                        }
                    }
                    Some("signature_delta") => {}
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = payload["index"].as_u64().unwrap_or(0) as usize;
                if let Some(AnthropicBlockState::ToolUse(tool_use)) = self.blocks.get_mut(&index) {
                    if !tool_use.partial_json.is_empty() {
                        tool_use.input = serde_json::from_str(&tool_use.partial_json)
                            .unwrap_or_else(|_| {
                                json!({
                                    "_raw": tool_use.partial_json,
                                })
                            });
                    }
                    events.push(ProviderEvent::ToolCallEnd {
                        id: tool_use.id.clone(),
                    });
                }
            }
            "message_delta" => {
                update_usage(&mut self.usage, &payload["usage"]);
                if let Some(stop_reason) = payload["delta"]["stop_reason"].as_str() {
                    self.stop_reason = map_stop_reason(stop_reason);
                }
            }
            "message_stop" => {
                events.push(ProviderEvent::Done(self.into_message()));
            }
            "error" => {
                let message = payload["error"]["message"]
                    .as_str()
                    .unwrap_or("Anthropic stream error")
                    .to_string();
                return Err(ProviderError::Stream(message));
            }
            _ => {}
        }

        Ok(events)
    }

    fn into_message(&mut self) -> AssistantMessage {
        self.finished = true;
        let content = self
            .blocks
            .values()
            .map(AnthropicBlockState::to_content_block)
            .collect();

        AssistantMessage {
            content,
            usage: std::mem::take(&mut self.usage),
            stop_reason: self.stop_reason,
            error_message: None,
            provider: self.model.provider.clone(),
            model: self.model.id.clone(),
            timestamp: now_millis(),
        }
    }
}

enum AnthropicBlockState {
    Text(String),
    Thinking(String),
    ToolUse(AnthropicToolUseState),
}

impl AnthropicBlockState {
    fn to_content_block(&self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text { text: text.clone() },
            Self::Thinking(thinking) => ContentBlock::Thinking {
                thinking: thinking.clone(),
            },
            Self::ToolUse(tool_use) => ContentBlock::ToolCall(ToolCall {
                id: tool_use.id.clone(),
                name: tool_use.name.clone(),
                arguments: tool_use.input.clone(),
            }),
        }
    }
}

struct AnthropicToolUseState {
    id: String,
    name: String,
    input: serde_json::Value,
    partial_json: String,
}

fn update_usage(usage: &mut Usage, value: &serde_json::Value) {
    usage.input_tokens = value["input_tokens"].as_u64().unwrap_or(usage.input_tokens);
    usage.output_tokens = value["output_tokens"]
        .as_u64()
        .unwrap_or(usage.output_tokens);
    usage.cache_read_tokens = value["cache_read_input_tokens"]
        .as_u64()
        .unwrap_or(usage.cache_read_tokens);
    usage.cache_write_tokens = value["cache_creation_input_tokens"]
        .as_u64()
        .unwrap_or(usage.cache_write_tokens);
    usage.total_tokens = Some(
        usage.input_tokens
            + usage.output_tokens
            + usage.cache_read_tokens
            + usage.cache_write_tokens,
    );
}

fn map_stop_reason(stop_reason: &str) -> StopReason {
    match stop_reason {
        "tool_use" => StopReason::ToolUse,
        "end_turn" | "max_tokens" => StopReason::Stop,
        _ => StopReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use anie_provider::{ApiKind, CostPerMillion};

    use super::*;

    fn sample_model() -> Model {
        Model {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            provider: "anthropic".into(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            context_window: 200_000,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: true,
            cost_per_million: CostPerMillion::zero(),
        }
    }

    #[test]
    fn convert_messages_batches_consecutive_tool_results() {
        let provider = AnthropicProvider::new();
        let messages = provider.convert_messages(&[
            Message::User(anie_protocol::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: 1,
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::Text { text: "a".into() }],
                details: serde_json::Value::Null,
                is_error: false,
                timestamp: 2,
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "call_2".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::Text { text: "b".into() }],
                details: serde_json::Value::Null,
                is_error: false,
                timestamp: 3,
            }),
        ]);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, "user");
        assert_eq!(
            messages[1]
                .content
                .as_array()
                .expect("tool result array")
                .len(),
            2
        );
    }

    #[test]
    fn convert_tools_adds_cache_control() {
        let provider = AnthropicProvider::new();
        let tools = provider.convert_tools(&[ToolDef {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }]);

        assert_eq!(tools[0]["cache_control"]["type"], json!("ephemeral"));
        assert_eq!(tools[0]["input_schema"]["type"], json!("object"));
    }

    #[test]
    fn parses_sse_events_into_provider_events() {
        let mut state = AnthropicStreamState::new(sample_model());
        assert!(matches!(
            state
                .process_event(
                    "message_start",
                    r#"{"message":{"usage":{"input_tokens":12}}}"#
                )
                .expect("message start")
                .first(),
            Some(ProviderEvent::Start)
        ));
        assert!(matches!(
            state
                .process_event(
                    "content_block_start",
                    r#"{"index":0,"content_block":{"type":"text"}}"#
                )
                .expect("text start")
                .len(),
            0
        ));
        assert!(matches!(
            state.process_event("content_block_delta", r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#)
                .expect("text delta")
                .first(),
            Some(ProviderEvent::TextDelta(text)) if text == "Hello"
        ));
        assert!(matches!(
            state.process_event("content_block_start", r#"{"index":1,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{}}}"#)
                .expect("tool start")
                .first(),
            Some(ProviderEvent::ToolCallStart(ToolCall { id, name, .. })) if id == "call_1" && name == "read"
        ));
        assert!(matches!(
            state.process_event("content_block_delta", r#"{"index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"Cargo.toml\"}"}}"#)
                .expect("tool delta")
                .first(),
            Some(ProviderEvent::ToolCallDelta { id, arguments_delta }) if id == "call_1" && arguments_delta.contains("Cargo.toml")
        ));
        assert!(matches!(
            state.process_event("content_block_stop", r#"{"index":1}"#)
                .expect("tool stop")
                .first(),
            Some(ProviderEvent::ToolCallEnd { id }) if id == "call_1"
        ));
        state
            .process_event(
                "message_delta",
                r#"{"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
            )
            .expect("message delta");
        let done = state
            .process_event("message_stop", "{}")
            .expect("message stop");
        let ProviderEvent::Done(message) = done.last().expect("done event") else {
            panic!("expected done event");
        };
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.usage.input_tokens, 12);
        assert_eq!(message.usage.output_tokens, 7);
        assert!(message.content.iter().any(|block| matches!(
            block,
            ContentBlock::ToolCall(ToolCall { id, arguments, .. }) if id == "call_1" && arguments == &json!({"path":"Cargo.toml"})
        )));
    }

    #[test]
    fn anthropic_provider_replays_thinking_blocks() {
        let provider = AnthropicProvider::new();
        assert!(provider.includes_thinking_in_replay());
    }

    #[test]
    fn thinking_config_maps_levels() {
        assert_eq!(thinking_config(ThinkingLevel::Off, 8_192), None);
        assert_eq!(
            thinking_config(ThinkingLevel::Low, 8_192).expect("low thinking")["budget_tokens"],
            json!(2_048)
        );
        assert_eq!(
            thinking_config(ThinkingLevel::Medium, 8_192).expect("medium thinking")["budget_tokens"],
            json!(4_096)
        );
        assert_eq!(
            thinking_config(ThinkingLevel::High, 8_192).expect("high thinking")["budget_tokens"],
            json!(8_192)
        );
    }
}
