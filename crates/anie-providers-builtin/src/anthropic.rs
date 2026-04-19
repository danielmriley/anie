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

use crate::{classify_http_error, http::shared_http_client, parse_retry_after, sse_stream};

/// Anthropic Messages API provider.
pub struct AnthropicProvider {
    client: reqwest::Client,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider, pulling the workspace-shared
    /// HTTP client when available.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: shared_http_client()
                .cloned()
                .unwrap_or_else(|_| crate::http::create_http_client()),
        }
    }

    // Test-visible so plan 03d / plan 06 fixture tests can assert on
    // outbound request shape without hitting the network.
    #[cfg(test)]
    pub(crate) fn build_request_body_for_test(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> serde_json::Value {
        self.build_request_body(model, context, options)
    }

    fn build_request_body(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), json!(model.id));
        let base_max = options.max_tokens.unwrap_or(model.max_tokens);
        if let Some((effective_max, thinking)) = thinking_config(options, model) {
            body.insert("max_tokens".into(), json!(effective_max));
            body.insert("thinking".into(), thinking);
            body.insert("temperature".into(), json!(1.0));
        } else {
            body.insert("max_tokens".into(), json!(base_max));
        }
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
        if let Some(thinking) = thinking_config(options, model) {
            body.insert("thinking".into(), thinking.1);
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
                .map_err(|error| ProviderError::Transport(error.to_string()))?;
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
                let event = event
                    .map_err(|error| ProviderError::MalformedStreamEvent(error.to_string()))?;
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

    fn requires_thinking_signature(&self) -> bool {
        true
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        let last = tools.len().saturating_sub(1);
        tools
            .iter()
            .enumerate()
            .map(|(i, tool)| {
                if !tools.is_empty() && i == last {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                        "cache_control": { "type": "ephemeral" },
                    })
                } else {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    })
                }
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
        .map(|block| match block {
            ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
            ContentBlock::Image { media_type, data } => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            }),
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                let mut block = serde_json::Map::new();
                block.insert("type".into(), json!("thinking"));
                block.insert("thinking".into(), json!(thinking));
                if let Some(signature) = signature {
                    block.insert("signature".into(), json!(signature));
                }
                serde_json::Value::Object(block)
            }
            ContentBlock::ToolCall(tool_call) => json!({
                "type": "tool_use",
                "id": tool_call.id,
                "name": tool_call.name,
                "input": tool_call.arguments,
            }),
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

fn thinking_config(options: &StreamOptions, model: &Model) -> Option<(u64, serde_json::Value)> {
    /// Minimum output tokens to reserve alongside the thinking budget.
    const MIN_OUTPUT_TOKENS: u64 = 1_024;

    // Fixed absolute budgets per level — not percentages of max_tokens.
    // Mirrors the pi-mono approach: budget is a constant, and max_tokens is
    // expanded to accommodate it rather than the budget being capped by it.
    let budget = match options.thinking {
        ThinkingLevel::Off => return None,
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High => 16_384,
    };

    // Expand max_tokens to fit both the thinking budget and some output,
    // then cap at the model's ceiling.
    let base = options.max_tokens.unwrap_or(model.max_tokens);
    let effective_max = (base + budget).min(model.max_tokens);

    // If the model cap is too tight to fit the budget plus the minimum
    // output reserve, shrink the budget to what remains.
    let budget = if effective_max <= budget + MIN_OUTPUT_TOKENS {
        effective_max.saturating_sub(MIN_OUTPUT_TOKENS)
    } else {
        budget
    };

    Some((
        effective_max,
        json!({ "type": "enabled", "budget_tokens": budget }),
    ))
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
        let payload: serde_json::Value = serde_json::from_str(data)
            .map_err(|error| ProviderError::InvalidStreamJson(error.to_string()))?;
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
                        // Some Anthropic SSE implementations include an
                        // initial `signature` on the start event; seed
                        // the state with it. Deltas accumulate onto the
                        // same buffer via `signature_delta`.
                        let signature = block
                            .get("signature")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        self.blocks.insert(
                            index,
                            AnthropicBlockState::Thinking(AnthropicThinkingState {
                                thinking: String::new(),
                                signature,
                            }),
                        );
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
                        if let Some(AnthropicBlockState::Thinking(state)) =
                            self.blocks.get_mut(&index)
                        {
                            state.thinking.push_str(&thinking);
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
                    Some("signature_delta") => {
                        // Opaque signature covering the thinking block;
                        // required on replay per Anthropic's contract.
                        // See docs/api_integrity_plans/01b_stream_capture.md.
                        let signature = delta
                            .get("signature")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default();
                        if let Some(AnthropicBlockState::Thinking(state)) =
                            self.blocks.get_mut(&index)
                        {
                            state.signature.push_str(signature);
                        }
                    }
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
                return Err(ProviderError::MalformedStreamEvent(message));
            }
            _ => {}
        }

        Ok(events)
    }

    // Consumes the current state by marking `finished` and taking owned
    // buffers out. Name kept as `into_*` for readability, even though the
    // receiver is `&mut self`, because the result is a materialized value.
    #[allow(clippy::wrong_self_convention)]
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
    Thinking(AnthropicThinkingState),
    ToolUse(AnthropicToolUseState),
}

struct AnthropicThinkingState {
    thinking: String,
    signature: String,
}

impl AnthropicBlockState {
    fn to_content_block(&self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text { text: text.clone() },
            Self::Thinking(state) => ContentBlock::Thinking {
                thinking: state.thinking.clone(),
                signature: (!state.signature.is_empty()).then(|| state.signature.clone()),
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
    fn convert_tools_adds_cache_control_only_to_last_tool() {
        let provider = AnthropicProvider::new();

        // Single tool: cache_control is on the only (last) entry.
        let single = provider.convert_tools(&[ToolDef {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
        }]);
        assert_eq!(single[0]["cache_control"]["type"], json!("ephemeral"));

        // Multiple tools: only the last one carries cache_control.
        let multi = provider.convert_tools(&[
            ToolDef {
                name: "read".into(),
                description: "Read".into(),
                parameters: json!({}),
            },
            ToolDef {
                name: "write".into(),
                description: "Write".into(),
                parameters: json!({}),
            },
            ToolDef {
                name: "edit".into(),
                description: "Edit".into(),
                parameters: json!({}),
            },
            ToolDef {
                name: "bash".into(),
                description: "Bash".into(),
                parameters: json!({}),
            },
        ]);
        assert!(
            multi[0].get("cache_control").is_none(),
            "first tool must not have cache_control"
        );
        assert!(
            multi[1].get("cache_control").is_none(),
            "middle tools must not have cache_control"
        );
        assert!(
            multi[2].get("cache_control").is_none(),
            "middle tools must not have cache_control"
        );
        assert_eq!(
            multi[3]["cache_control"]["type"],
            json!("ephemeral"),
            "last tool must have cache_control"
        );

        // Empty list: no panic.
        assert!(provider.convert_tools(&[]).is_empty());
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
    fn captures_signature_delta_on_thinking_block() {
        let mut state = AnthropicStreamState::new(sample_model());
        state
            .process_event(
                "message_start",
                r#"{"message":{"usage":{"input_tokens":10}}}"#,
            )
            .expect("message start");
        state
            .process_event(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking"}}"#,
            )
            .expect("thinking start");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"reasoning"}}"#,
            )
            .expect("thinking delta");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"signature_delta","signature":"SIG_abc"}}"#,
            )
            .expect("signature delta");
        state
            .process_event("content_block_stop", r#"{"index":0}"#)
            .expect("block stop");
        state
            .process_event(
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"}}"#,
            )
            .expect("message delta");
        let done = state
            .process_event("message_stop", "{}")
            .expect("message stop");

        let ProviderEvent::Done(message) = done.last().expect("done event") else {
            panic!("expected done event");
        };
        assert!(
            message.content.iter().any(|block| matches!(
                block,
                ContentBlock::Thinking { thinking, signature: Some(sig) }
                    if thinking == "reasoning" && sig == "SIG_abc"
            )),
            "expected thinking block with signature captured"
        );
    }

    #[test]
    fn concatenates_split_signature_deltas() {
        let mut state = AnthropicStreamState::new(sample_model());
        state
            .process_event(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking"}}"#,
            )
            .expect("thinking start");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"signature_delta","signature":"PART_A_"}}"#,
            )
            .expect("signature delta A");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"signature_delta","signature":"PART_B"}}"#,
            )
            .expect("signature delta B");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#,
            )
            .expect("thinking delta");
        state
            .process_event("content_block_stop", r#"{"index":0}"#)
            .expect("block stop");

        let message = state.into_message();
        assert!(
            message.content.iter().any(|block| matches!(
                block,
                ContentBlock::Thinking { signature: Some(sig), .. } if sig == "PART_A_PART_B"
            )),
            "expected concatenated signature"
        );
    }

    #[test]
    fn uses_content_block_start_signature_when_present() {
        let mut state = AnthropicStreamState::new(sample_model());
        state
            .process_event(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking","signature":"SEED"}}"#,
            )
            .expect("thinking start with signature");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#,
            )
            .expect("thinking delta");
        state
            .process_event("content_block_stop", r#"{"index":0}"#)
            .expect("block stop");

        let message = state.into_message();
        assert!(
            message.content.iter().any(|block| matches!(
                block,
                ContentBlock::Thinking { signature: Some(sig), .. } if sig == "SEED"
            )),
            "expected seeded signature"
        );
    }

    #[test]
    fn thinking_block_serialization_includes_signature_when_present() {
        let signed = content_blocks_to_anthropic(&[ContentBlock::Thinking {
            thinking: "r".into(),
            signature: Some("SIG".into()),
        }]);
        assert_eq!(signed[0]["type"], json!("thinking"));
        assert_eq!(signed[0]["thinking"], json!("r"));
        assert_eq!(signed[0]["signature"], json!("SIG"));
    }

    #[test]
    fn thinking_block_serialization_omits_signature_when_absent() {
        let unsigned = content_blocks_to_anthropic(&[ContentBlock::Thinking {
            thinking: "r".into(),
            signature: None,
        }]);
        assert_eq!(unsigned[0]["type"], json!("thinking"));
        assert_eq!(unsigned[0]["thinking"], json!("r"));
        assert!(
            unsigned[0].get("signature").is_none(),
            "unsigned thinking must omit the signature key"
        );
    }

    #[test]
    fn anthropic_provider_requires_thinking_signature() {
        let provider = AnthropicProvider::new();
        assert!(provider.requires_thinking_signature());
    }

    #[test]
    fn cache_control_marker_count_stays_bounded_with_many_tools() {
        // Regression guard for an earlier production 400: when every
        // tool carried `cache_control`, 5+ tools tripped Anthropic's
        // "max 4 blocks" limit. Only the last tool should carry
        // cache_control; system prompt carries one more. Total <= 2,
        // well under the API limit of 4.
        let provider = AnthropicProvider::new();
        let tools: Vec<ToolDef> = (0..10)
            .map(|i| ToolDef {
                name: format!("tool_{i}"),
                description: format!("tool {i}"),
                parameters: json!({"type": "object"}),
            })
            .collect();
        let context = LlmContext {
            system_prompt: "hello".into(),
            messages: Vec::new(),
            tools,
        };
        let options = StreamOptions::default();
        let body = provider.build_request_body_for_test(&sample_model(), &context, &options);

        let count = count_cache_control_markers(&body);
        assert!(
            count <= 4,
            "cache_control marker count must stay <= 4 (got {count}): {body}"
        );
        assert_eq!(
            count, 2,
            "expected exactly one marker on system and one on the last tool"
        );
    }

    fn count_cache_control_markers(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                let here = usize::from(map.contains_key("cache_control"));
                let in_children: usize = map.values().map(count_cache_control_markers).sum();
                here + in_children
            }
            serde_json::Value::Array(items) => {
                items.iter().map(count_cache_control_markers).sum()
            }
            _ => 0,
        }
    }

    #[test]
    fn unsigned_thinking_block_has_none_signature() {
        let mut state = AnthropicStreamState::new(sample_model());
        state
            .process_event(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking"}}"#,
            )
            .expect("thinking start");
        state
            .process_event(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"x"}}"#,
            )
            .expect("thinking delta");
        state
            .process_event("content_block_stop", r#"{"index":0}"#)
            .expect("block stop");

        let message = state.into_message();
        assert!(
            message.content.iter().any(|block| matches!(
                block,
                ContentBlock::Thinking { signature: None, .. }
            )),
            "expected unsigned thinking to yield None signature"
        );
    }

    #[test]
    fn thinking_config_maps_levels() {
        fn opts(level: ThinkingLevel) -> StreamOptions {
            StreamOptions {
                thinking: level,
                ..Default::default()
            }
        }
        fn model_with(max_tokens: u64) -> Model {
            Model {
                id: "claude-haiku-4-5-20251001".into(),
                name: "Haiku".into(),
                provider: "anthropic".into(),
                api: anie_provider::ApiKind::AnthropicMessages,
                base_url: "https://api.anthropic.com".into(),
                context_window: 200_000,
                max_tokens,
                supports_reasoning: true,
                reasoning_capabilities: None,
                supports_images: true,
                cost_per_million: anie_provider::CostPerMillion::zero(),
            }
        }

        // Off ⇒ no thinking block
        assert!(thinking_config(&opts(ThinkingLevel::Off), &model_with(64_000)).is_none());

        // With a large model (64 k), budgets are the fixed absolute values.
        let model = model_with(64_000);
        let (_, low) = thinking_config(&opts(ThinkingLevel::Low), &model).unwrap();
        let (_, med) = thinking_config(&opts(ThinkingLevel::Medium), &model).unwrap();
        let (eff_max, high) = thinking_config(&opts(ThinkingLevel::High), &model).unwrap();
        assert_eq!(low["budget_tokens"], json!(2_048));
        assert_eq!(med["budget_tokens"], json!(8_192));
        assert_eq!(high["budget_tokens"], json!(16_384));
        // budget must always be strictly less than effective_max_tokens
        assert!(high["budget_tokens"].as_u64().unwrap() < eff_max);

        // With a small model (max_tokens = 8 192), the budget is capped so
        // that at least MIN_OUTPUT_TOKENS (1 024) remain for the response.
        let small = model_with(8_192);
        let (eff, high_small) = thinking_config(&opts(ThinkingLevel::High), &small).unwrap();
        assert_eq!(eff, 8_192);
        assert!(high_small["budget_tokens"].as_u64().unwrap() < eff);
        assert_eq!(high_small["budget_tokens"], json!(8_192 - 1_024));
    }
}
