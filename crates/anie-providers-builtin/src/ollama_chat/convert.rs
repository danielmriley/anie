use serde_json::{Map, Value, json};

use anie_protocol::{ContentBlock, Message, ToolCall};
use anie_provider::{LlmContext, LlmMessage, Model, StreamOptions, ThinkingLevel};

pub(super) fn build_request_body(
    model: &Model,
    context: &LlmContext,
    options: &StreamOptions,
) -> serde_json::Value {
    let num_ctx = options.num_ctx_override.unwrap_or(model.context_window);
    let mut body = json!({
        "model": model.id,
        "messages": request_messages(context),
        "stream": true,
        "options": {
            "num_ctx": num_ctx,
        },
    });
    if model.reasoning_capabilities.is_some() {
        body["think"] = json!(options.thinking != ThinkingLevel::Off);
    }
    if !context.tools.is_empty() {
        body["tools"] = json!(crate::tool_schema::openai_function_schema(&context.tools));
    }
    body
}

fn request_messages(context: &LlmContext) -> Vec<serde_json::Value> {
    let mut messages = Vec::with_capacity(context.messages.len() + 1);
    if !context.system_prompt.trim().is_empty() {
        messages.push(json!({
            "role": "system",
            "content": context.system_prompt,
        }));
    }
    messages.extend(context.messages.iter().map(llm_message_to_ollama_message));
    messages
}

fn llm_message_to_ollama_message(message: &LlmMessage) -> serde_json::Value {
    let mut out = Map::new();
    out.insert("role".into(), json!(message.role));
    if let Some(object) = message.content.as_object() {
        out.extend(object.clone());
    } else {
        out.insert(
            "content".into(),
            json!(message.content.as_str().unwrap_or_default()),
        );
    }
    Value::Object(out)
}

pub(super) fn convert_messages(messages: &[Message]) -> Vec<LlmMessage> {
    messages
        .iter()
        .map(|message| match message {
            Message::User(user) => LlmMessage {
                role: "user".into(),
                content: serde_json::Value::String(text_content(&user.content)),
            },
            Message::Assistant(assistant) => assistant_message_to_ollama(&assistant.content),
            Message::ToolResult(tool_result) => LlmMessage {
                role: "tool".into(),
                content: json!({
                    "content": text_content(&tool_result.content),
                    "tool_name": tool_result.tool_name,
                }),
            },
            Message::Custom(custom) => LlmMessage {
                role: "custom".into(),
                content: custom.content.clone(),
            },
        })
        .collect()
}

fn assistant_message_to_ollama(content: &[ContentBlock]) -> LlmMessage {
    let text = text_content(content);
    let tool_calls = tool_calls(content);
    if tool_calls.is_empty() {
        return LlmMessage {
            role: "assistant".into(),
            content: Value::String(text),
        };
    }

    LlmMessage {
        role: "assistant".into(),
        content: json!({
            "content": text,
            "tool_calls": tool_calls,
        }),
    }
}

fn tool_calls(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(tool_call) => Some(ollama_tool_call(tool_call)),
            _ => None,
        })
        .collect()
}

fn ollama_tool_call(tool_call: &ToolCall) -> Value {
    json!({
        "function": {
            "name": tool_call.name,
            "arguments": tool_call.arguments,
        },
    })
}

fn text_content(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        let ContentBlock::Text { text } = block else {
            continue;
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(text);
    }
    out
}

#[cfg(test)]
mod tests {
    use anie_protocol::{
        AssistantMessage, StopReason, ToolResultMessage, Usage, UserMessage, now_millis,
    };
    use anie_provider::{
        ApiKind, CostPerMillion, ModelCompat, ReasoningCapabilities, ReasoningControlMode,
        ReasoningOutputMode, ThinkingRequestMode,
    };

    use super::*;

    fn sample_model(context_window: u64) -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OllamaChatApi,
            base_url: "http://localhost:11434".into(),
            context_window,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn thinking_model() -> Model {
        let mut model = sample_model(32_768);
        model.reasoning_capabilities = Some(ReasoningCapabilities {
            control: Some(ReasoningControlMode::Native),
            output: Some(ReasoningOutputMode::Separated),
            tags: None,
            request_mode: Some(ThinkingRequestMode::EnableThinkingFlag),
        });
        model
    }

    fn sample_context() -> LlmContext {
        LlmContext {
            system_prompt: "You are helpful".into(),
            messages: convert_messages(&[Message::User(UserMessage {
                content: vec![ContentBlock::Text { text: "hi".into() }],
                timestamp: now_millis(),
            })]),
            tools: Vec::new(),
        }
    }

    #[test]
    fn request_body_contains_model_stream_messages_and_num_ctx() {
        let body = build_request_body(
            &sample_model(32_768),
            &sample_context(),
            &StreamOptions::default(),
        );

        assert_eq!(body["model"], "qwen3:32b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["num_ctx"], 32_768);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn request_body_num_ctx_equals_model_context_window() {
        let body = build_request_body(
            &sample_model(16_384),
            &sample_context(),
            &StreamOptions::default(),
        );

        assert_eq!(body["options"]["num_ctx"], 16_384);
    }

    #[test]
    fn ollama_chat_body_prefers_num_ctx_override_over_context_window() {
        let body = build_request_body(
            &sample_model(32_768),
            &sample_context(),
            &StreamOptions {
                num_ctx_override: Some(16_384),
                ..StreamOptions::default()
            },
        );

        assert_eq!(body["options"]["num_ctx"], 16_384);
    }

    #[test]
    fn ollama_chat_body_uses_context_window_when_override_is_none() {
        let body = build_request_body(
            &sample_model(32_768),
            &sample_context(),
            &StreamOptions::default(),
        );

        assert_eq!(body["options"]["num_ctx"], 32_768);
    }

    #[test]
    fn request_body_includes_think_true_for_low_medium_high() {
        for thinking in [
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ] {
            let body = build_request_body(
                &thinking_model(),
                &sample_context(),
                &StreamOptions {
                    thinking,
                    ..StreamOptions::default()
                },
            );

            assert_eq!(body["think"], true);
        }
    }

    #[test]
    fn request_body_includes_think_false_for_off() {
        let body = build_request_body(
            &thinking_model(),
            &sample_context(),
            &StreamOptions::default(),
        );

        assert_eq!(body["think"], false);
    }

    #[test]
    fn request_body_omits_think_field_for_non_thinking_capable_model() {
        let body = build_request_body(
            &sample_model(32_768),
            &sample_context(),
            &StreamOptions {
                thinking: ThinkingLevel::High,
                ..StreamOptions::default()
            },
        );

        assert!(body.get("think").is_none());
    }

    #[test]
    fn request_body_serializes_tools_with_openai_function_schema() {
        let mut context = sample_context();
        context.tools = vec![anie_protocol::ToolDef {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
        }];

        let body = build_request_body(&thinking_model(), &context, &StreamOptions::default());

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    }

    #[test]
    fn ollama_tool_result_replay_serializes_without_requiring_synthesized_id() {
        let messages = convert_messages(&[Message::ToolResult(ToolResultMessage {
            tool_call_id: "synthetic-id-is-not-sent".into(),
            tool_name: "read_file".into(),
            content: vec![ContentBlock::Text {
                text: "contents".into(),
            }],
            details: Value::Null,
            is_error: false,
            timestamp: now_millis(),
        })]);
        let context = LlmContext {
            system_prompt: String::new(),
            messages,
            tools: Vec::new(),
        };

        let body = build_request_body(&sample_model(32_768), &context, &StreamOptions::default());

        assert_eq!(body["messages"][0]["role"], "tool");
        assert_eq!(body["messages"][0]["tool_name"], "read_file");
        assert_eq!(body["messages"][0]["content"], "contents");
        assert!(body["messages"][0].get("tool_call_id").is_none());
    }

    #[test]
    fn ollama_assistant_tool_call_and_tool_result_replay_shape_matches_fixture() {
        let messages = convert_messages(&[
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "synthesized-on-previous-response".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "Cargo.toml"}),
                })],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                provider: "ollama".into(),
                model: "qwen3:32b".into(),
                timestamp: now_millis(),
                reasoning_details: None,
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "synthesized-on-previous-response".into(),
                tool_name: "read_file".into(),
                content: vec![ContentBlock::Text {
                    text: "workspace".into(),
                }],
                details: Value::Null,
                is_error: false,
                timestamp: now_millis(),
            }),
        ]);
        let context = LlmContext {
            system_prompt: String::new(),
            messages,
            tools: Vec::new(),
        };

        let body = build_request_body(&sample_model(32_768), &context, &StreamOptions::default());

        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"],
            json!({
                "name": "read_file",
                "arguments": {
                    "path": "Cargo.toml",
                },
            })
        );
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_name"], "read_file");
        assert!(body["messages"][0]["tool_calls"][0].get("id").is_none());
    }
}
