use serde_json::json;

use anie_protocol::{ContentBlock, Message};
use anie_provider::{LlmContext, LlmMessage, Model};

pub(super) fn build_request_body(model: &Model, context: &LlmContext) -> serde_json::Value {
    json!({
        "model": model.id,
        "messages": request_messages(context),
        "stream": true,
        "options": {
            "num_ctx": model.context_window,
        },
    })
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
    json!({
        "role": message.role,
        "content": message.content.as_str().unwrap_or_default(),
    })
}

pub(super) fn convert_messages(messages: &[Message]) -> Vec<LlmMessage> {
    messages
        .iter()
        .map(|message| match message {
            Message::User(user) => LlmMessage {
                role: "user".into(),
                content: serde_json::Value::String(text_content(&user.content)),
            },
            Message::Assistant(assistant) => LlmMessage {
                role: "assistant".into(),
                content: serde_json::Value::String(text_content(&assistant.content)),
            },
            Message::ToolResult(tool_result) => LlmMessage {
                role: "tool".into(),
                content: serde_json::Value::String(text_content(&tool_result.content)),
            },
            Message::Custom(custom) => LlmMessage {
                role: "custom".into(),
                content: custom.content.clone(),
            },
        })
        .collect()
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
    use anie_protocol::{UserMessage, now_millis};
    use anie_provider::{ApiKind, CostPerMillion, ModelCompat};

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
        let body = build_request_body(&sample_model(32_768), &sample_context());

        assert_eq!(body["model"], "qwen3:32b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["num_ctx"], 32_768);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn request_body_num_ctx_equals_model_context_window() {
        let body = build_request_body(&sample_model(16_384), &sample_context());

        assert_eq!(body["options"]["num_ctx"], 16_384);
    }
}
