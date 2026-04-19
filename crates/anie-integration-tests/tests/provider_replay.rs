//! Multi-turn replay fidelity tests + cross-provider invariants.
//!
//! Covers both plan 06 (scenario-driven fixtures for the replay
//! boundary) and plan 03d (table-driven invariants that every
//! provider must satisfy on a shared multi-turn fixture).
//!
//! The Anthropic provider exposes `build_request_body_for_test` via
//! the `test-utils` feature so this file can inspect the serialized
//! request body without hitting the network. OpenAI exposes the same.

use anie_protocol::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolResultMessage, Usage,
    UserMessage,
};
use anie_provider::{
    ApiKind, CostPerMillion, LlmContext, Model, Provider, ReplayCapabilities, StreamOptions,
};
use anie_providers_builtin::{AnthropicProvider, OpenAIProvider};
use serde_json::json;

// ============================================================
// Shared fixture: a realistic two-turn conversation with signed
// thinking, a tool call, a tool result, and a follow-up user turn.
// ============================================================

fn signed_thinking_fixture() -> Vec<Message> {
    vec![
        Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "compute 2+2".into(),
            }],
            timestamp: 1,
        }),
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "addition is trivial".into(),
                    signature: Some("SIG_1".into()),
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_xyz".into(),
                    name: "calculator".into(),
                    arguments: json!({ "op": "add", "a": 2, "b": 2 }),
                }),
            ],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            timestamp: 2,
        }),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_xyz".into(),
            tool_name: "calculator".into(),
            content: vec![ContentBlock::Text { text: "4".into() }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: 3,
        }),
        Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: "now try 3+3".into(),
            }],
            timestamp: 4,
        }),
    ]
}

fn anthropic_model() -> Model {
    Model {
        id: "claude-sonnet-4-6".into(),
        name: "Claude".into(),
        provider: "anthropic".into(),
        api: ApiKind::AnthropicMessages,
        base_url: "https://api.anthropic.com".into(),
        context_window: 200_000,
        max_tokens: 8_192,
        supports_reasoning: true,
        reasoning_capabilities: None,
        supports_images: true,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: Some(ReplayCapabilities {
            requires_thinking_signature: true,
            supports_redacted_thinking: true,
            supports_encrypted_reasoning: false,
        }),
    }
}

fn openai_model() -> Model {
    Model {
        id: "gpt-4o".into(),
        name: "GPT-4o".into(),
        provider: "openai".into(),
        api: ApiKind::OpenAICompletions,
        base_url: "https://api.openai.com/v1".into(),
        context_window: 128_000,
        max_tokens: 16_384,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: true,
        cost_per_million: CostPerMillion::zero(),
        replay_capabilities: None,
    }
}

fn build_anthropic_body(messages: Vec<Message>) -> serde_json::Value {
    let provider = AnthropicProvider::new();
    let llm_messages = provider.convert_messages(&messages);
    let ctx = LlmContext {
        system_prompt: String::new(),
        messages: llm_messages,
        tools: Vec::new(),
    };
    provider.build_request_body_for_test(&anthropic_model(), &ctx, &StreamOptions::default())
}

fn build_openai_body(messages: Vec<Message>) -> serde_json::Value {
    let provider = OpenAIProvider::new();
    let llm_messages = provider.convert_messages(&messages);
    let ctx = LlmContext {
        system_prompt: String::new(),
        messages: llm_messages,
        tools: Vec::new(),
    };
    provider.build_request_body_for_test(&openai_model(), &ctx, &StreamOptions::default())
}

fn count_cache_control_markers(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Object(map) => {
            let here = usize::from(map.contains_key("cache_control"));
            let in_children: usize = map.values().map(count_cache_control_markers).sum();
            here + in_children
        }
        serde_json::Value::Array(items) => items.iter().map(count_cache_control_markers).sum(),
        _ => 0,
    }
}

// ============================================================
// Plan 06 fixtures — one test per replay scenario
// ============================================================

#[test]
fn anthropic_thinking_signature_replay_emits_signature_on_wire() {
    let body = build_anthropic_body(signed_thinking_fixture());

    // Find the assistant turn; its first content block is thinking.
    let messages = body["messages"].as_array().expect("messages array");
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn");
    let thinking_block = assistant["content"]
        .as_array()
        .expect("content array")
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("thinking block");
    assert_eq!(thinking_block["signature"], json!("SIG_1"));
    assert_eq!(thinking_block["thinking"], json!("addition is trivial"));
}

#[test]
fn anthropic_redacted_thinking_replay_preserves_data_verbatim() {
    let mut fixture = signed_thinking_fixture();
    // Swap the signed thinking for a redacted block.
    if let Some(Message::Assistant(a)) = fixture.get_mut(1) {
        a.content[0] = ContentBlock::RedactedThinking {
            data: "OPAQUE_ENCRYPTED_PAYLOAD".into(),
        };
    }

    let body = build_anthropic_body(fixture);
    let messages = body["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn");
    let redacted = assistant["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "redacted_thinking")
        .expect("redacted_thinking block");
    assert_eq!(redacted["data"], json!("OPAQUE_ENCRYPTED_PAYLOAD"));
}

#[test]
fn openai_strips_reasoning_on_replay() {
    let body = build_openai_body(signed_thinking_fixture());
    let serialized = serde_json::to_string(&body).unwrap().to_lowercase();

    // Thinking text "addition is trivial" must not appear in the
    // outbound request — OpenAI chat-completions doesn't round-trip
    // reasoning. Likewise no `reasoning`, no `<think>`, no signature.
    assert!(
        !serialized.contains("addition is trivial"),
        "openai replayed thinking content: {serialized}"
    );
    assert!(!serialized.contains("sig_1"));
    assert!(!serialized.contains("<think>"));
}

#[test]
fn openai_tool_call_id_roundtrips() {
    let body = build_openai_body(signed_thinking_fixture());
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        serialized.contains("call_xyz"),
        "openai dropped tool_call_id: {serialized}"
    );
}

#[test]
fn anthropic_tool_call_id_roundtrips() {
    let body = build_anthropic_body(signed_thinking_fixture());
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        serialized.contains("call_xyz"),
        "anthropic dropped tool_call_id: {serialized}"
    );
}

// ============================================================
// Plan 03d — cross-provider invariant suite
// ============================================================

#[test]
fn cache_control_marker_count_bounded_across_providers() {
    // Anthropic uses cache_control; OpenAI doesn't. Both must stay
    // at or below the API's 4-marker cap. This is a regression
    // guard for the earlier "Found 5" production 400.
    let anthropic_body = build_anthropic_body(signed_thinking_fixture());
    let openai_body = build_openai_body(signed_thinking_fixture());

    let anthropic_count = count_cache_control_markers(&anthropic_body);
    let openai_count = count_cache_control_markers(&openai_body);

    assert!(
        anthropic_count <= 4,
        "anthropic body has {anthropic_count} cache_control markers (max 4)"
    );
    assert_eq!(
        openai_count, 0,
        "openai must not emit cache_control markers"
    );
}

#[test]
fn no_null_opaque_field_artifacts_in_serialized_body() {
    // If a block has signature: None we must omit the key, not
    // emit `"signature": null`. Same invariant for redacted
    // `data`, etc.
    let anthropic_body = build_anthropic_body(signed_thinking_fixture());
    let anthropic_str = serde_json::to_string(&anthropic_body).unwrap();
    assert!(
        !anthropic_str.contains("\"signature\":null"),
        "anthropic emitted a null signature: {anthropic_str}"
    );
    assert!(
        !anthropic_str.contains("\"data\":null"),
        "anthropic emitted a null data field: {anthropic_str}"
    );
}

#[test]
fn required_opaque_fields_present_per_model_capabilities() {
    // For any model whose ReplayCapabilities declares
    // requires_thinking_signature=true, a thinking block with a
    // signature MUST round-trip to the wire with that signature
    // present.
    let body = build_anthropic_body(signed_thinking_fixture());
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(
        serialized.contains("SIG_1"),
        "anthropic with requires_thinking_signature=true dropped a provided signature"
    );
}

#[test]
fn anthropic_drops_unsigned_thinking_from_replay_via_sanitizer() {
    // If a legacy (unsigned) thinking block is in the context, the
    // Provider-level convert_messages on its own will preserve the
    // structure — it's the agent-loop sanitizer that drops it. The
    // corresponding end-to-end test is in session_resume.rs
    // (legacy_unsigned_thinking_is_dropped_before_replay).
    //
    // Here we assert the narrower invariant: even if an unsigned
    // block DOES reach convert_messages (test path, no sanitizer),
    // the Anthropic serializer still produces a structurally valid
    // block — it just won't carry a signature key, which Anthropic
    // would then 400. The sanitizer is what prevents that in
    // production.
    let mut fixture = signed_thinking_fixture();
    if let Some(Message::Assistant(a)) = fixture.get_mut(1) {
        a.content[0] = ContentBlock::Thinking {
            thinking: "unsigned reasoning".into(),
            signature: None,
        };
    }
    let body = build_anthropic_body(fixture);
    let serialized = serde_json::to_string(&body).unwrap();
    assert!(!serialized.contains("\"signature\":"));
}

#[test]
fn body_is_valid_json_and_parses_back() {
    for body in [
        build_anthropic_body(signed_thinking_fixture()),
        build_openai_body(signed_thinking_fixture()),
    ] {
        let serialized = serde_json::to_string(&body).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reparsed, body);
    }
}

#[test]
fn conversation_shape_and_roles_preserved_across_providers() {
    // Each provider must preserve the user/assistant/tool_result
    // sequence, even if the exact content changes (e.g., OpenAI
    // drops thinking, Anthropic keeps it).
    let anthropic_body = build_anthropic_body(signed_thinking_fixture());
    let openai_body = build_openai_body(signed_thinking_fixture());

    let anthropic_roles: Vec<&str> = anthropic_body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["role"].as_str())
        .collect();
    let openai_roles: Vec<&str> = openai_body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["role"].as_str())
        .collect();

    // Anthropic: user, assistant, user(tool_result wrapped as user in Anthropic), user
    assert!(anthropic_roles.starts_with(&["user", "assistant"]));
    // OpenAI: user, assistant, tool, user
    assert_eq!(openai_roles, vec!["user", "assistant", "tool", "user"]);
}
