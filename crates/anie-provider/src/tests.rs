use futures::StreamExt;

use anie_protocol::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolDef, Usage, UserMessage,
};

use crate::{
    ApiKind, CostPerMillion, LlmContext, Model, ModelCompat, Provider, ProviderError,
    ProviderEvent, ProviderRegistry, ReasoningCapabilities, ReasoningControlMode,
    ReasoningOutputMode, ReasoningTags, StreamOptions, ThinkingRequestMode,
    mock::{MockProvider, MockStreamScript},
};

fn sample_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock Model".into(),
        provider: "mock".into(),
        api: ApiKind::OpenAICompletions,
        base_url: "http://localhost".into(),
        context_window: 128_000,
        max_tokens: 8_192,
        supports_reasoning: false,
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
        messages: vec![],
        tools: vec![ToolDef {
            name: "read".into(),
            description: "Read file contents".into(),
            parameters: serde_json::json!({"type": "object"}),
        }],
    }
}

fn api_kind_wire_name(api: ApiKind) -> &'static str {
    match api {
        ApiKind::AnthropicMessages => "AnthropicMessages",
        ApiKind::OpenAICompletions => "OpenAICompletions",
        ApiKind::OpenAIResponses => "OpenAIResponses",
        ApiKind::GoogleGenerativeAI => "GoogleGenerativeAI",
        ApiKind::OllamaChatApi => "OllamaChatApi",
    }
}

fn final_message() -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        provider: "mock".into(),
        model: "mock-model".into(),
        timestamp: 1,
        reasoning_details: None,
    }
}

#[test]
fn ollama_chat_api_variant_round_trips_serde_name() {
    let json = serde_json::to_string(&ApiKind::OllamaChatApi).expect("serialize api kind");
    assert_eq!(json, "\"OllamaChatApi\"");

    let roundtrip: ApiKind = serde_json::from_str(&json).expect("deserialize api kind");
    assert_eq!(roundtrip, ApiKind::OllamaChatApi);

    let mut model = sample_model();
    model.provider = "ollama".into();
    model.api = ApiKind::OllamaChatApi;
    model.base_url = "http://localhost:11434".into();

    let toml = toml::to_string(&model).expect("serialize model to toml");
    assert!(toml.contains("api = \"OllamaChatApi\""));

    let roundtrip: Model = toml::from_str(&toml).expect("deserialize model from toml");
    assert_eq!(roundtrip, model);
}

#[test]
fn api_kind_exhaustive_match_still_compiles() {
    assert_eq!(api_kind_wire_name(ApiKind::OllamaChatApi), "OllamaChatApi");
}

#[test]
fn model_serde_is_backward_compatible_without_reasoning_capabilities() {
    let model: Model = serde_json::from_value(serde_json::json!({
        "id": "mock-model",
        "name": "Mock Model",
        "provider": "mock",
        "api": "OpenAICompletions",
        "base_url": "http://localhost",
        "context_window": 128000,
        "max_tokens": 8192,
        "supports_reasoning": false,
        "supports_images": false,
        "cost_per_million": {
            "input": 0.0,
            "output": 0.0,
            "cache_read": 0.0,
            "cache_write": 0.0
        }
    }))
    .expect("deserialize model");

    assert_eq!(model.id, "mock-model");
    assert_eq!(model.reasoning_capabilities, None);
    assert!(!model.supports_reasoning);
}

#[test]
fn model_serde_roundtrips_reasoning_capabilities() {
    let mut model = sample_model();
    model.reasoning_capabilities = Some(ReasoningCapabilities {
        control: Some(ReasoningControlMode::Native),
        output: Some(ReasoningOutputMode::Tagged),
        tags: Some(ReasoningTags {
            open: "<think>".into(),
            close: "</think>".into(),
        }),
        request_mode: Some(ThinkingRequestMode::ReasoningEffort),
    });
    model.supports_reasoning = true;

    let json = serde_json::to_value(&model).expect("serialize model");
    let roundtrip: Model = serde_json::from_value(json).expect("deserialize model");

    assert_eq!(roundtrip, model);
}

#[test]
fn registry_registers_and_looks_up_providers() {
    let mut registry = ProviderRegistry::new();
    registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            final_message(),
        )])),
    );

    assert!(registry.get(&ApiKind::OpenAICompletions).is_some());
    assert!(registry.get(&ApiKind::AnthropicMessages).is_none());
}

#[tokio::test]
async fn registry_streams_using_registered_provider() {
    let mut registry = ProviderRegistry::new();
    registry.register(
        ApiKind::OpenAICompletions,
        Box::new(MockProvider::new(vec![MockStreamScript::from_message(
            final_message(),
        )])),
    );

    let mut stream = registry
        .stream(&sample_model(), sample_context(), StreamOptions::default())
        .expect("stream available");

    let event = stream.next().await.expect("event").expect("provider ok");
    assert!(matches!(event, ProviderEvent::Done(_)));
}

#[test]
fn registry_returns_structured_error_when_provider_missing() {
    let registry = ProviderRegistry::new();
    let Err(error) = registry.stream(&sample_model(), sample_context(), StreamOptions::default())
    else {
        panic!("missing provider should error")
    };

    assert!(matches!(
        error,
        ProviderError::RequestBuild(message) if message.contains("No provider registered")
    ));
}

#[tokio::test]
async fn mock_provider_can_stream_tool_calls_and_text() {
    let tool_call = ToolCall {
        id: "call_1".into(),
        name: "read".into(),
        arguments: serde_json::Value::Null,
    };
    let provider = MockProvider::new(vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::Start),
        Ok(ProviderEvent::TextDelta("hello".into())),
        Ok(ProviderEvent::ToolCallStart(tool_call.clone())),
        Ok(ProviderEvent::ToolCallDelta {
            id: tool_call.id.clone(),
            arguments_delta: "{\"path\": \"src/lib.rs\"}".into(),
        }),
        Ok(ProviderEvent::ToolCallEnd {
            id: tool_call.id.clone(),
        }),
        Ok(ProviderEvent::Done(final_message())),
    ])]);

    let mut stream = provider
        .stream(&sample_model(), sample_context(), StreamOptions::default())
        .expect("mock stream");

    let mut seen_tool_start = false;
    let mut seen_text = false;
    while let Some(item) = stream.next().await {
        match item.expect("provider ok") {
            ProviderEvent::TextDelta(text) => {
                assert_eq!(text, "hello");
                seen_text = true;
            }
            ProviderEvent::ToolCallStart(call) => {
                assert_eq!(call, tool_call);
                seen_tool_start = true;
            }
            _ => {}
        }
    }

    assert!(seen_text);
    assert!(seen_tool_start);
}

#[tokio::test]
async fn mock_provider_can_emit_mid_stream_errors() {
    let provider = MockProvider::new(vec![MockStreamScript::new(vec![
        Ok(ProviderEvent::Start),
        Err(ProviderError::MalformedStreamEvent("socket dropped".into())),
    ])]);

    let mut stream = provider
        .stream(&sample_model(), sample_context(), StreamOptions::default())
        .expect("mock stream");

    assert!(matches!(
        stream.next().await,
        Some(Ok(ProviderEvent::Start))
    ));
    assert!(matches!(
        stream.next().await,
        Some(Err(ProviderError::MalformedStreamEvent(message))) if message == "socket dropped"
    ));
}

#[test]
fn provider_error_retry_after_accessor_is_stable() {
    assert_eq!(
        ProviderError::RateLimited {
            retry_after_ms: Some(500)
        }
        .retry_after_ms(),
        Some(500)
    );
    assert_eq!(
        ProviderError::Transport("dns".into()).retry_after_ms(),
        None
    );
    assert_eq!(
        ProviderError::Http {
            status: 503,
            body: "down".into()
        }
        .retry_after_ms(),
        None
    );
}

#[test]
fn mock_provider_message_and_tool_conversion_are_stable() {
    let provider = MockProvider::new(vec![]);
    let messages = provider.convert_messages(&[Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: "hello".into(),
        }],
        timestamp: 1,
    })]);
    let tools = provider.convert_tools(&[ToolDef {
        name: "read".into(),
        description: "Read file contents".into(),
        parameters: serde_json::json!({"type": "object"}),
    }]);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], serde_json::json!("read"));
}
