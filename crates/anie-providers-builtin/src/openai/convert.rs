//! Protocol ↔ OpenAI wire-format message conversion.
//!
//! These helpers translate between anie's canonical `Message` /
//! `ContentBlock` types and the OpenAI chat-completions request shape.
//! They are used by the Provider trait impl when building request bodies.

use serde_json::json;

use anie_protocol::{AssistantMessage, ContentBlock};
use anie_provider::LlmMessage;

/// Convert an `AssistantMessage` into an OpenAI-shaped `LlmMessage`.
///
/// Returns `None` when the message has no text *and* no tool calls — an
/// empty assistant turn has no stable replay representation. Thinking
/// blocks are intentionally dropped: OpenAI-compatible backends do not
/// round-trip historical reasoning as assistant content (see
/// `docs/completed/reasoning_fix_plan.md` phase 1 sub-step C).
pub(super) fn assistant_message_to_openai_llm_message(
    assistant_message: &AssistantMessage,
) -> Option<LlmMessage> {
    // Plan 08 PR-A: direct-buffer join — two-pass single-
    // allocation build instead of Vec<&str> + join.
    let text = {
        let mut total = 0usize;
        let mut count = 0usize;
        for block in &assistant_message.content {
            if let ContentBlock::Text { text } = block {
                if count > 0 {
                    total += 1; // '\n' separator
                }
                total += text.len();
                count += 1;
            }
        }
        let mut out = String::with_capacity(total);
        let mut first = true;
        for block in &assistant_message.content {
            if let ContentBlock::Text { text } = block {
                if !first {
                    out.push('\n');
                }
                out.push_str(text);
                first = false;
            }
        }
        out
    };
    let tool_calls = assistant_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(tool_call) => Some(json!({
                "id": tool_call.id,
                "type": "function",
                "function": {
                    "name": tool_call.name,
                    "arguments": serde_json::to_string(&tool_call.arguments).unwrap_or_else(|_| "null".into()),
                }
            })),
            _ => None,
        })
        .collect::<Vec<_>>();

    if text.is_empty() && tool_calls.is_empty() {
        return None;
    }

    let mut payload = serde_json::Map::new();
    payload.insert(
        "content".into(),
        if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(text)
        },
    );
    if !tool_calls.is_empty() {
        payload.insert("tool_calls".into(), serde_json::Value::Array(tool_calls));
    }
    if let Some(details) = assistant_message.reasoning_details.as_ref()
        && !details.is_empty()
    {
        // Round-trip OpenRouter's encrypted reasoning wrapper so
        // the upstream's chain-of-thought stays connected across
        // turns. Only populated on models whose catalog entry
        // declared `supports_reasoning_details_replay`; the
        // streaming layer drops it everywhere else so this branch
        // is a no-op for non-opt-in providers.
        payload.insert(
            "reasoning_details".into(),
            serde_json::Value::Array(details.clone()),
        );
    }

    Some(LlmMessage {
        role: "assistant".into(),
        content: serde_json::Value::Object(payload),
    })
}

/// Convert an `LlmMessage` (neutral intermediate form) into the
/// OpenAI chat-completions wire shape. Handles `assistant`, `tool`,
/// and pass-through for `system` / `user`.
pub(super) fn llm_message_to_openai_message(message: &LlmMessage) -> serde_json::Value {
    match message.role.as_str() {
        "assistant" => {
            if let Some(content) = message.content.as_object() {
                let mut payload = serde_json::Map::new();
                payload.insert("role".into(), json!("assistant"));
                payload.insert(
                    "content".into(),
                    content
                        .get("content")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                );
                if let Some(tool_calls) = content.get("tool_calls") {
                    payload.insert("tool_calls".into(), tool_calls.clone());
                }
                if let Some(details) = content.get("reasoning_details") {
                    payload.insert("reasoning_details".into(), details.clone());
                }
                serde_json::Value::Object(payload)
            } else {
                json!({ "role": "assistant", "content": message.content })
            }
        }
        "tool" => {
            if let Some(content) = message.content.as_object() {
                json!({
                    "role": "tool",
                    "tool_call_id": content.get("tool_call_id").cloned().unwrap_or(serde_json::Value::Null),
                    "content": content.get("content").cloned().unwrap_or(serde_json::Value::String(String::new())),
                })
            } else {
                json!({ "role": "tool", "content": message.content })
            }
        }
        _ => json!({ "role": message.role, "content": message.content }),
    }
}

/// Convert user content to OpenAI chat-completions content.
///
/// Text-only messages keep the legacy string shape for broad
/// OpenAI-compatible backend support. When a user message includes an
/// image, OpenAI requires ordered content parts; in that shape, anie's
/// user-side thinking blocks pass through as text parts to preserve the
/// previous `join_text_content` behavior. Redacted thinking remains
/// provider-opaque and is not sent to OpenAI-compatible backends.
pub(super) fn user_content_to_openai(content: &[ContentBlock]) -> serde_json::Value {
    let has_image = content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    if !has_image {
        return serde_json::Value::String(join_text_content(content));
    }

    let parts = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({
                "type": "text",
                "text": text,
            })),
            ContentBlock::Thinking { thinking, .. } => Some(json!({
                "type": "text",
                "text": thinking,
            })),
            ContentBlock::Image { media_type, data } => Some(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{media_type};base64,{data}"),
                },
            })),
            ContentBlock::RedactedThinking { .. } | ContentBlock::ToolCall(_) => None,
        })
        .collect();

    serde_json::Value::Array(parts)
}

/// Flatten the text-bearing content of a message for wire formats that
/// expect a single `content` string. Thinking blocks are joined inline;
/// images are serialized as `[image:MIME;base64,…]` placeholders;
/// tool-call blocks are skipped.
pub(super) fn join_text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::Thinking { thinking, .. } => Some(thinking.clone()),
            // OpenAI chat-completions does not round-trip reasoning
            // as assistant content, so redacted blocks can't be sent
            // back verbatim the way Anthropic requires. Drop them.
            ContentBlock::RedactedThinking { .. } => None,
            ContentBlock::Image { media_type, data } => {
                Some(format!("[image:{media_type};base64,{data}]"))
            }
            ContentBlock::ToolCall(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use anie_protocol::{AssistantMessage, ContentBlock, Message, StopReason, ToolCall, Usage};
    use anie_provider::Provider;

    use super::{
        assistant_message_to_openai_llm_message, llm_message_to_openai_message,
        user_content_to_openai,
    };
    use crate::OpenAIProvider;

    #[test]
    fn converts_messages_for_openai_chat_completions() {
        let provider = OpenAIProvider::new();
        let messages = provider.convert_messages(&[
            Message::User(anie_protocol::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
                timestamp: 1,
            }),
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({ "path": "src/main.rs" }),
                })],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                provider: "openai".into(),
                model: "gpt-4o".into(),
                timestamp: 2,
                reasoning_details: None,
            }),
            Message::ToolResult(anie_protocol::ToolResultMessage {
                tool_call_id: "call_1".into(),
                tool_name: "read".into(),
                content: vec![ContentBlock::Text {
                    text: "fn main() {}".into(),
                }],
                details: serde_json::Value::Null,
                is_error: false,
                timestamp: 3,
            }),
        ]);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content["content"], serde_json::Value::Null);
        assert!(messages[1].content["tool_calls"].is_array());
        assert_eq!(messages[2].role, "tool");
    }

    #[test]
    fn skips_empty_assistant_messages_when_converting_messages() {
        let provider = OpenAIProvider::new();
        let messages = provider.convert_messages(&[
            Message::User(anie_protocol::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "first".into(),
                }],
                timestamp: 1,
            }),
            Message::Assistant(AssistantMessage {
                content: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                provider: "ollama".into(),
                model: "qwen3.5:9b".into(),
                timestamp: 2,
                reasoning_details: None,
            }),
            Message::User(anie_protocol::UserMessage {
                content: vec![ContentBlock::Text {
                    text: "second".into(),
                }],
                timestamp: 3,
            }),
        ]);

        assert_eq!(messages.len(), 2);
        assert!(messages.iter().all(|message| message.role == "user"));
    }

    #[test]
    fn reasoning_only_assistant_messages_are_omitted_from_openai_replay() {
        let provider = OpenAIProvider::new();
        let messages = provider.convert_messages(&[Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Thinking {
                thinking: "plan first".into(),
                signature: None,
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "ollama".into(),
            model: "qwen3:32b".into(),
            timestamp: 1,
            reasoning_details: None,
        })]);

        assert!(messages.is_empty());
    }

    #[test]
    fn text_only_user_content_keeps_legacy_string_shape_and_includes_thinking_text() {
        let content = vec![
            ContentBlock::Text {
                text: "question".into(),
            },
            ContentBlock::Thinking {
                thinking: "scratchpad".into(),
                signature: None,
            },
        ];

        assert_eq!(
            user_content_to_openai(&content),
            json!("question\nscratchpad")
        );
    }

    #[test]
    fn image_user_content_uses_ordered_openai_content_parts() {
        let content = vec![
            ContentBlock::Text {
                text: "what is this?".into(),
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo=".into(),
            },
            ContentBlock::Thinking {
                thinking: "inspect image".into(),
                signature: None,
            },
        ];

        assert_eq!(
            user_content_to_openai(&content),
            json!([
                { "type": "text", "text": "what is this?" },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,iVBORw0KGgo="
                    }
                },
                { "type": "text", "text": "inspect image" }
            ])
        );
    }

    #[test]
    fn thinking_is_omitted_but_text_and_tools_preserved_in_openai_replay() {
        let provider = OpenAIProvider::new();
        let messages = provider.convert_messages(&[Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "plan first".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "final answer".into(),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({ "path": "README.md" }),
                }),
            ],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "ollama".into(),
            model: "qwen3:32b".into(),
            timestamp: 1,
            reasoning_details: None,
        })]);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "assistant");
        assert_eq!(messages[0].content["content"], json!("final answer"));
        assert_eq!(messages[0].content["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(
            messages[0].content["tool_calls"][0]["function"]["name"],
            json!("read")
        );
        assert!(!messages[0].content.to_string().contains("plan first"));
    }

    #[test]
    fn assistant_message_reasoning_details_round_trip_via_llm_message() {
        use anie_protocol::{StopReason, Usage};
        let details = vec![json!({
            "type": "reasoning.encrypted",
            "id": "call_abc",
            "data": "OPAQUE",
        })];
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "openrouter".into(),
            model: "openai/o3".into(),
            timestamp: 1,
            reasoning_details: Some(details.clone()),
        };

        let llm = assistant_message_to_openai_llm_message(&assistant).expect("llm message");
        assert_eq!(llm.content["reasoning_details"], json!(details));

        // Now flatten through the outbound converter: the replayed
        // wire message should carry the reasoning_details array
        // alongside content / tool_calls.
        let wire = llm_message_to_openai_message(&llm);
        assert_eq!(wire["role"], "assistant");
        assert_eq!(wire["reasoning_details"], json!(details));
    }

    #[test]
    fn assistant_message_without_reasoning_details_omits_field() {
        use anie_protocol::{StopReason, Usage};
        let assistant = AssistantMessage {
            content: vec![ContentBlock::Text { text: "hi".into() }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            timestamp: 1,
            reasoning_details: None,
        };
        let llm = assistant_message_to_openai_llm_message(&assistant).expect("llm message");
        assert!(llm.content.get("reasoning_details").is_none());
    }
}
