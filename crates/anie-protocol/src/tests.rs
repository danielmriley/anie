use serde_json::json;

use crate::{
    AssistantMessage, ContentBlock, Cost, CustomMessage, Message, StopReason, ToolCall, ToolDef,
    ToolResult, ToolResultMessage, Usage, UserMessage,
};

fn roundtrip<T>(value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(value).expect("serialize");
    let decoded: T = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(&decoded, value);
}

fn sample_usage() -> Usage {
    Usage {
        input_tokens: 12,
        output_tokens: 34,
        cache_read_tokens: 5,
        cache_write_tokens: 6,
        total_tokens: Some(57),
        cost: Cost {
            input: 0.1,
            output: 0.2,
            cache_read: 0.03,
            cache_write: 0.04,
            total: 0.37,
        },
    }
}

fn sample_tool_call() -> ToolCall {
    ToolCall {
        id: "call_123".into(),
        name: "read".into(),
        arguments: json!({"path": "src/main.rs", "offset": 10, "nested": {"flag": true}}),
    }
}

#[test]
fn user_message_roundtrip() {
    roundtrip(&Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "hello".into(),
        }],
        timestamp: 1,
    }));
}

#[test]
fn assistant_message_roundtrip() {
    roundtrip(&Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::Text { text: "hi".into() }],
        usage: sample_usage(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "test-model".into(),
        timestamp: 2,
    }));
}

#[test]
fn tool_result_message_roundtrip() {
    roundtrip(&Message::ToolResult(ToolResultMessage {
        tool_call_id: "call_123".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::Text {
            text: "fn main() {}".into(),
        }],
        details: json!({"path": "src/main.rs"}),
        is_error: false,
        timestamp: 3,
    }));
}

#[test]
fn custom_message_roundtrip() {
    roundtrip(&Message::Custom(CustomMessage {
        custom_type: "extension.note".into(),
        content: json!({"ok": true}),
        timestamp: 4,
    }));
}

#[test]
fn message_role_tags_use_expected_names() {
    let json = serde_json::to_value(Message::User(UserMessage {
        content: vec![],
        timestamp: 42,
    }))
    .expect("serialize message");
    assert_eq!(json["role"], json!("user"));
}

#[test]
fn text_content_block_roundtrip() {
    roundtrip(&ContentBlock::Text {
        text: "hello".into(),
    });
}

#[test]
fn image_content_block_roundtrip() {
    roundtrip(&ContentBlock::Image {
        media_type: "image/png".into(),
        data: "YmFzZTY0".into(),
    });
}

#[test]
fn thinking_content_block_roundtrip() {
    roundtrip(&ContentBlock::Thinking {
        thinking: "let me think".into(),
        signature: None,
    });
}

#[test]
fn thinking_content_block_with_signature_roundtrip() {
    roundtrip(&ContentBlock::Thinking {
        thinking: "let me think".into(),
        signature: Some("SIG_abc123".into()),
    });
}

#[test]
fn thinking_content_block_deserializes_without_signature_field() {
    let old_json = r#"{"type":"thinking","thinking":"hmm"}"#;
    let block: ContentBlock = serde_json::from_str(old_json).unwrap();
    assert!(matches!(
        &block,
        ContentBlock::Thinking { thinking, signature: None } if thinking == "hmm"
    ));
}

#[test]
fn thinking_content_block_without_signature_reserializes_cleanly() {
    let block = ContentBlock::Thinking {
        thinking: "hmm".into(),
        signature: None,
    };
    let serialized = serde_json::to_string(&block).unwrap();
    assert_eq!(serialized, r#"{"type":"thinking","thinking":"hmm"}"#);
    assert!(!serialized.contains("signature"));
}

#[test]
fn thinking_content_block_with_signature_emits_signature_field() {
    let block = ContentBlock::Thinking {
        thinking: "hmm".into(),
        signature: Some("SIG".into()),
    };
    let serialized = serde_json::to_string(&block).unwrap();
    assert!(serialized.contains(r#""signature":"SIG""#));
    let parsed: ContentBlock = serde_json::from_str(&serialized).unwrap();
    assert_eq!(parsed, block);
}

#[test]
fn redacted_thinking_content_block_roundtrip() {
    roundtrip(&ContentBlock::RedactedThinking {
        data: "opaque-base64-payload".into(),
    });
}

#[test]
fn redacted_thinking_uses_camelcase_wire_tag() {
    let block = ContentBlock::RedactedThinking {
        data: "x".into(),
    };
    let serialized = serde_json::to_string(&block).unwrap();
    assert!(serialized.contains(r#""type":"redactedThinking""#));
    assert!(serialized.contains(r#""data":"x""#));
}

#[test]
fn tool_call_content_block_roundtrip() {
    roundtrip(&ContentBlock::ToolCall(sample_tool_call()));
}

#[test]
fn nested_tool_call_arguments_roundtrip() {
    roundtrip(&sample_tool_call());
}

#[test]
fn empty_content_arrays_roundtrip() {
    roundtrip(&Message::User(UserMessage {
        content: vec![],
        timestamp: 55,
    }));
}

#[test]
fn unicode_text_roundtrip() {
    roundtrip(&Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "héllo 👋 — こんにちは".into(),
        }],
        timestamp: 56,
    }));
}

#[test]
fn null_tool_call_arguments_roundtrip() {
    roundtrip(&ToolCall {
        id: "call_null".into(),
        name: "noop".into(),
        arguments: serde_json::Value::Null,
    });
}

#[test]
fn tool_def_roundtrip() {
    roundtrip(&ToolDef {
        name: "read".into(),
        description: "Read a file".into(),
        parameters: json!({"type": "object", "required": ["path"]}),
    });
}

#[test]
fn tool_result_roundtrip() {
    roundtrip(&ToolResult {
        content: vec![ContentBlock::Text {
            text: "done".into(),
        }],
        details: json!({"path": "src/lib.rs"}),
    });
}

#[test]
fn usage_roundtrip() {
    roundtrip(&sample_usage());
}

#[test]
fn cost_roundtrip() {
    roundtrip(&sample_usage().cost);
}

#[test]
fn stop_reason_stop_roundtrip() {
    roundtrip(&StopReason::Stop);
}

#[test]
fn stop_reason_tool_use_roundtrip() {
    roundtrip(&StopReason::ToolUse);
}

#[test]
fn stop_reason_error_roundtrip() {
    roundtrip(&StopReason::Error);
}

#[test]
fn stop_reason_aborted_roundtrip() {
    roundtrip(&StopReason::Aborted);
}

#[test]
fn assistant_message_error_message_is_optional() {
    let value = serde_json::to_value(AssistantMessage {
        content: vec![ContentBlock::Text {
            text: "oops".into(),
        }],
        usage: sample_usage(),
        stop_reason: StopReason::Error,
        error_message: None,
        provider: "mock".into(),
        model: "m".into(),
        timestamp: 99,
    })
    .expect("serialize assistant");
    assert!(value.get("error_message").is_none());
}
