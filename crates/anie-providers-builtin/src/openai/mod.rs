//! OpenAI-compatible chat-completions provider.
//!
//! Composed of four submodules:
//! - `tagged_reasoning` — <think>…</think> extraction
//! - `streaming`        — SSE → `ProviderEvent` reassembly
//! - `convert`          — protocol `Message` ↔ OpenAI wire format
//! - `reasoning_strategy` — native-reasoning request-side policy
//!
//! This file hosts the `Provider` impl itself and the small retry
//! loop around `send_stream_request`. Per-submodule unit tests
//! live alongside their submodule.

use async_stream::try_stream;
use futures::StreamExt;
use serde_json::json;

use anie_protocol::{Message, ToolDef};
use anie_provider::{
    LlmContext, LlmMessage, MaxTokensField, Model, ModelCompat, Provider, ProviderError,
    ProviderEvent, ProviderStream, ReasoningControlMode, StreamOptions, ThinkingLevel,
    ThinkingRequestMode,
};

use crate::openrouter::{insert_anthropic_cache_control, needs_anthropic_cache_control};
use crate::{http::shared_http_client, parse_retry_after, sse_stream};

mod convert;
mod reasoning_strategy;
mod streaming;
mod tagged_reasoning;

use convert::{
    assistant_message_to_openai_llm_message, join_text_content, llm_message_to_openai_message,
    user_content_to_openai,
};
use reasoning_strategy::{
    NativeReasoningRequestStrategy, OpenAiCompatibleBackend, classify_openai_http_error,
    effective_max_tokens, effective_reasoning_capabilities, is_local_openai_compatible_target,
    is_native_reasoning_compatibility_error, local_reasoning_prompt_steering,
    openai_compatible_backend, reasoning_effort,
};
use streaming::OpenAiStreamState;

/// OpenAI-compatible chat-completions provider implementation.
#[derive(Clone)]
pub struct OpenAIProvider {
    client: reqwest::Client,
}

impl OpenAIProvider {
    /// Create a new provider, using the workspace-shared HTTP client
    /// when available. Falls back to a fresh client (which will panic
    /// the same way legacy code did) if the shared init fails — this
    /// keeps `::new()` infallible.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: shared_http_client()
                .cloned()
                .unwrap_or_else(|_| crate::http::create_http_client()),
        }
    }

    fn effective_system_prompt(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Option<String> {
        let system_prompt =
            Some(context.system_prompt.clone()).filter(|prompt| !prompt.trim().is_empty());
        let prompt_steering = if is_local_openai_compatible_target(model) {
            Some(local_reasoning_prompt_steering(options.thinking))
        } else {
            None
        };

        match (system_prompt, prompt_steering) {
            (Some(system_prompt), Some(prompt_steering)) => {
                Some(format!("{system_prompt}\n\n{prompt_steering}"))
            }
            (Some(system_prompt), None) => Some(system_prompt),
            (None, Some(prompt_steering)) => Some(prompt_steering.to_string()),
            (None, None) => None,
        }
    }

    fn openai_request_messages(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Vec<serde_json::Value> {
        let mut messages = Vec::with_capacity(context.messages.len() + 1);
        if let Some(system_prompt) = self.effective_system_prompt(model, context, options) {
            messages.push(json!({
                "role": "system",
                "content": system_prompt,
            }));
        }
        messages.extend(context.messages.iter().map(llm_message_to_openai_message));
        messages
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn build_request_body(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
        include_stream_options: bool,
    ) -> serde_json::Value {
        self.build_request_body_with_native_reasoning_strategy(
            model,
            context,
            options,
            include_stream_options,
            NativeReasoningRequestStrategy::NoNativeFields,
        )
    }

    /// Test-only: expose the serialized request body so integration
    /// tests can assert on outbound wire shape without hitting the
    /// network. See plan 06 for the multi-turn replay harness.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn build_request_body_for_test(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> serde_json::Value {
        self.build_request_body(model, context, options, true)
    }

    fn build_request_body_with_native_reasoning_strategy(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
        include_stream_options: bool,
        native_reasoning_strategy: NativeReasoningRequestStrategy,
    ) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        body.insert("model".into(), serde_json::Value::String(model.id.clone()));
        body.insert("stream".into(), serde_json::Value::Bool(true));
        let mut messages = self.openai_request_messages(model, context, options);
        if needs_anthropic_cache_control(model) {
            insert_anthropic_cache_control(&mut messages);
        }
        body.insert("messages".into(), serde_json::Value::Array(messages));

        let tools = self.convert_tools(&context.tools);
        if !tools.is_empty() {
            body.insert("tools".into(), serde_json::Value::Array(tools));
        }
        if include_stream_options {
            body.insert("stream_options".into(), json!({ "include_usage": true }));
        }
        if let Some(temperature) = options.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if let Some(max_tokens) = effective_max_tokens(model, options) {
            // OpenAI renamed `max_tokens` to `max_completion_tokens`
            // for the o-series and GPT-5 models; the old name 400s
            // on those endpoints. Per-model selection via the
            // compat blob. Default is the legacy name for
            // backward compat with older OpenAI-compat servers.
            let field_name = match &model.compat {
                ModelCompat::OpenAICompletions(compat) => match compat.max_tokens_field {
                    Some(MaxTokensField::MaxCompletionTokens) => "max_completion_tokens",
                    Some(MaxTokensField::MaxTokens) | None => "max_tokens",
                },
                ModelCompat::None => "max_tokens",
            };
            body.insert(field_name.into(), json!(max_tokens));
        }
        if let ModelCompat::OpenAICompletions(compat) = &model.compat
            && let Some(routing) = compat.openrouter_routing.as_ref()
            && let Ok(value) = serde_json::to_value(routing)
            && value.as_object().is_some_and(|map| !map.is_empty())
        {
            // OpenRouter-only: surfaces the user's routing
            // preferences as the top-level `provider` field.
            // Ignored by every other OpenAI-compatible backend.
            body.insert("provider".into(), value);
        }

        match native_reasoning_strategy {
            NativeReasoningRequestStrategy::NestedReasoning => {
                // Nested-reasoning backends (LM Studio, OpenRouter)
                // expect an explicit `{effort: "none"}` to disable
                // reasoning rather than a field omission, matching
                // pi's OpenRouter mapping.
                let effort = reasoning_effort(options.thinking).unwrap_or("none");
                body.insert("reasoning".into(), json!({ "effort": effort }));
            }
            NativeReasoningRequestStrategy::TopLevelReasoningEffort => {
                if let Some(effort) = reasoning_effort(options.thinking) {
                    body.insert("reasoning_effort".into(), json!(effort));
                }
            }
            NativeReasoningRequestStrategy::EnableThinkingFlag { nested } => {
                // Boolean disable signal for vLLM / SGLang Qwen3+
                // and Z.ai GLM. `Off` is the only reason to pick
                // this path on the Off side — it's what lets the
                // user actually turn thinking off on a capable
                // model. Any non-Off level sends `true`.
                let enabled = options.thinking != ThinkingLevel::Off;
                if nested {
                    body.insert(
                        "chat_template_kwargs".into(),
                        json!({ "enable_thinking": enabled }),
                    );
                } else {
                    body.insert("enable_thinking".into(), json!(enabled));
                }
            }
            NativeReasoningRequestStrategy::NoNativeFields => {
                if let Some(effort) = reasoning_effort(options.thinking)
                    && model.supports_reasoning
                    && !is_local_openai_compatible_target(model)
                {
                    body.insert("reasoning_effort".into(), json!(effort));
                    body.insert("reasoning".into(), json!({ "summary": "auto" }));
                }
            }
        }

        serde_json::Value::Object(body)
    }

    async fn send_request(
        client: reqwest::Client,
        url: String,
        body: serde_json::Value,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut request = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(api_key) = &options.api_key {
            request = request.bearer_auth(api_key);
        }
        for (name, value) in &options.headers {
            request = request.header(name, value);
        }
        request
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))
    }

    async fn send_stream_request_once(
        client: reqwest::Client,
        url: String,
        initial_body: serde_json::Value,
        fallback_body: serde_json::Value,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        let response =
            Self::send_request(client.clone(), url.clone(), initial_body, options).await?;
        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let retry_after = parse_retry_after(&response);
        let body = response.text().await.unwrap_or_default();
        if status == reqwest::StatusCode::BAD_REQUEST && body.contains("stream_options") {
            let fallback = Self::send_request(client, url, fallback_body, options).await?;
            if fallback.status().is_success() {
                return Ok(fallback);
            }

            let status = fallback.status();
            let retry_after = parse_retry_after(&fallback);
            let body = fallback.text().await.unwrap_or_default();
            return Err(classify_openai_http_error(status, &body, retry_after));
        }

        Err(classify_openai_http_error(status, &body, retry_after))
    }

    async fn send_stream_request(
        &self,
        model: &Model,
        context: &LlmContext,
        url: &str,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        for strategy in self.native_reasoning_request_strategies(model, options) {
            let initial_body = self.build_request_body_with_native_reasoning_strategy(
                model, context, options, true, strategy,
            );
            let fallback_body = self.build_request_body_with_native_reasoning_strategy(
                model, context, options, false, strategy,
            );
            match Self::send_stream_request_once(
                self.client.clone(),
                url.to_string(),
                initial_body,
                fallback_body,
                options,
            )
            .await
            {
                Ok(response) => return Ok(response),
                Err(error)
                    if strategy != NativeReasoningRequestStrategy::NoNativeFields
                        && is_native_reasoning_compatibility_error(&error) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }

        Err(ProviderError::RequestBuild(
            "no compatible OpenAI request strategy available".into(),
        ))
    }

    fn native_reasoning_request_strategies(
        &self,
        model: &Model,
        options: &StreamOptions,
    ) -> Vec<NativeReasoningRequestStrategy> {
        let capabilities = effective_reasoning_capabilities(model);

        // Nested reasoning is used by both LM Studio (local) and
        // OpenRouter (hosted) and always sends an effort value —
        // `{effort: "none"}` for `Off`. Honor a declared
        // `NestedReasoning` mode before applying the local-only
        // gate so hosted OpenRouter catalog entries can take this
        // path.
        if let Some(caps) = capabilities.as_ref()
            && caps.control == Some(ReasoningControlMode::Native)
            && caps.request_mode == Some(ThinkingRequestMode::NestedReasoning)
        {
            return vec![
                NativeReasoningRequestStrategy::NestedReasoning,
                NativeReasoningRequestStrategy::NoNativeFields,
            ];
        }

        // EnableThinkingFlag variants always emit the boolean —
        // `false` for `Off`, `true` otherwise. They must bypass
        // the generic Off-short-circuit below so the upstream
        // (vLLM/SGLang Qwen3+, Z.ai GLM) actually receives the
        // disable signal instead of an omitted field.
        if let Some(caps) = capabilities.as_ref()
            && caps.control == Some(ReasoningControlMode::Native)
            && caps.request_mode == Some(ThinkingRequestMode::EnableThinkingFlag)
        {
            return vec![
                NativeReasoningRequestStrategy::EnableThinkingFlag { nested: false },
                NativeReasoningRequestStrategy::NoNativeFields,
            ];
        }
        if let Some(caps) = capabilities.as_ref()
            && caps.control == Some(ReasoningControlMode::Native)
            && caps.request_mode == Some(ThinkingRequestMode::ChatTemplateEnableThinking)
        {
            return vec![
                NativeReasoningRequestStrategy::EnableThinkingFlag { nested: true },
                NativeReasoningRequestStrategy::NoNativeFields,
            ];
        }

        if options.thinking == ThinkingLevel::Off || !is_local_openai_compatible_target(model) {
            return vec![NativeReasoningRequestStrategy::NoNativeFields];
        }

        let Some(capabilities) = capabilities else {
            return vec![NativeReasoningRequestStrategy::NoNativeFields];
        };
        if capabilities.control != Some(ReasoningControlMode::Native) {
            return vec![NativeReasoningRequestStrategy::NoNativeFields];
        }

        let strategy = match capabilities.request_mode {
            Some(ThinkingRequestMode::ReasoningEffort) => {
                Some(NativeReasoningRequestStrategy::TopLevelReasoningEffort)
            }
            Some(ThinkingRequestMode::NestedReasoning) => {
                // Handled above; unreachable but kept for completeness.
                Some(NativeReasoningRequestStrategy::NestedReasoning)
            }
            Some(ThinkingRequestMode::EnableThinkingFlag) => {
                // Handled above; unreachable but kept for completeness.
                Some(NativeReasoningRequestStrategy::EnableThinkingFlag { nested: false })
            }
            Some(ThinkingRequestMode::ChatTemplateEnableThinking) => {
                // Handled above; unreachable but kept for completeness.
                Some(NativeReasoningRequestStrategy::EnableThinkingFlag { nested: true })
            }
            Some(ThinkingRequestMode::PromptSteering) => None,
            None => match openai_compatible_backend(model) {
                OpenAiCompatibleBackend::LmStudio => {
                    Some(NativeReasoningRequestStrategy::NestedReasoning)
                }
                OpenAiCompatibleBackend::Ollama
                | OpenAiCompatibleBackend::Vllm
                | OpenAiCompatibleBackend::UnknownLocal => {
                    Some(NativeReasoningRequestStrategy::TopLevelReasoningEffort)
                }
                OpenAiCompatibleBackend::Hosted => None,
            },
        };

        match strategy {
            Some(strategy) => vec![strategy, NativeReasoningRequestStrategy::NoNativeFields],
            None => vec![NativeReasoningRequestStrategy::NoNativeFields],
        }
    }
}

impl Default for OpenAIProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for OpenAIProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let provider = self.clone();
        let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
        let request_model = model.clone();
        let state_model = model.clone();

        let stream = try_stream! {
            let response = provider
                .send_stream_request(&request_model, &context, &url, &options)
                .await?;

            yield ProviderEvent::Start;
            let mut events = sse_stream(response);
            let mut state = OpenAiStreamState::new(&state_model);
            while let Some(event) = events.next().await {
                let event = event
                    .map_err(|error| ProviderError::MalformedStreamEvent(error.to_string()))?;
                for provider_event in state.process_event(&event.data)? {
                    yield provider_event;
                }
            }

            if !state.is_finished() {
                for provider_event in state.finish_stream()? {
                    yield provider_event;
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::User(user_message) => Some(LlmMessage {
                    role: "user".into(),
                    content: user_content_to_openai(&user_message.content),
                }),
                Message::Assistant(assistant_message) => {
                    assistant_message_to_openai_llm_message(assistant_message)
                }
                Message::ToolResult(tool_result) => Some(LlmMessage {
                    role: "tool".into(),
                    content: json!({
                        "tool_call_id": tool_result.tool_call_id,
                        "content": join_text_content(&tool_result.content),
                    }),
                }),
                Message::Custom(custom_message) => Some(LlmMessage {
                    role: "custom".into(),
                    content: custom_message.content.clone(),
                }),
            })
            .collect()
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use anie_protocol::{AssistantMessage, ContentBlock, StopReason, ToolCall, Usage};
    use anie_provider::{
        ApiKind, ModelCompat, ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode,
        ThinkingRequestMode,
    };

    use super::*;

    fn sample_model() -> Model {
        Model {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".into(),
            context_window: 128_000,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: true,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn sample_heuristic_local_model(provider: &str, base_url: &str, id: &str) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            provider: provider.into(),
            api: ApiKind::OpenAICompletions,
            base_url: base_url.into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn sample_local_model() -> Model {
        sample_heuristic_local_model("ollama", "http://localhost:11434/v1", "qwen3:32b")
    }

    fn sample_native_local_model(
        provider: &str,
        base_url: &str,
        request_mode: ThinkingRequestMode,
    ) -> Model {
        Model {
            id: "reasoner".into(),
            name: "Reasoner".into(),
            provider: provider.into(),
            api: ApiKind::OpenAICompletions,
            base_url: base_url.into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Tagged),
                tags: None,
                request_mode: Some(request_mode),
            }),
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    #[test]
    fn openai_provider_does_not_replay_thinking_blocks() {
        let provider = OpenAIProvider::new();
        assert!(!provider.includes_thinking_in_replay());
    }

    #[test]
    fn builtin_openai_models_do_not_require_thinking_signature() {
        // OpenAI chat-completions does not round-trip opaque
        // signatures; the catalog must not declare the requirement.
        use crate::builtin_models;
        let models = builtin_models();
        let gpt_4o = models
            .iter()
            .find(|m| m.id == "gpt-4o")
            .expect("gpt-4o model");
        let caps = gpt_4o.effective_replay_capabilities();
        assert!(!caps.requires_thinking_signature);
    }

    #[test]
    fn request_body_prepends_system_prompt_and_preserves_message_order() {
        let provider = OpenAIProvider::new();
        let model = sample_model();
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
        let body = provider.build_request_body(
            &model,
            &LlmContext {
                system_prompt: "sys".into(),
                messages,
                tools: vec![],
            },
            &StreamOptions::default(),
            true,
        );

        assert_eq!(body["messages"].as_array().map(Vec::len), Some(4));
        assert_eq!(
            body["messages"][0],
            json!({ "role": "system", "content": "sys" })
        );
        assert_eq!(body["messages"][1]["role"], json!("user"));
        assert_eq!(body["messages"][2]["role"], json!("assistant"));
        assert_eq!(body["messages"][2]["content"], serde_json::Value::Null);
        assert!(body["messages"][2]["tool_calls"].is_array());
        assert_eq!(body["messages"][3]["role"], json!("tool"));
        assert_eq!(body["model"], json!("gpt-4o"));
        assert!(body.get("stream_options").is_some());
    }

    #[test]
    fn request_body_omits_blank_system_prompt() {
        let provider = OpenAIProvider::new();
        let model = sample_model();
        let body = provider.build_request_body(
            &model,
            &LlmContext {
                system_prompt: "  \n\t  ".into(),
                messages: vec![LlmMessage {
                    role: "user".into(),
                    content: json!("hello"),
                }],
                tools: vec![],
            },
            &StreamOptions::default(),
            true,
        );

        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            body["messages"][0],
            json!({ "role": "user", "content": "hello" })
        );
    }

    #[test]
    fn local_effective_system_prompt_varies_by_thinking_level() {
        let provider = OpenAIProvider::new();
        let model = sample_local_model();
        let context = LlmContext {
            system_prompt: String::new(),
            messages: vec![],
            tools: vec![],
        };

        let off = provider
            .effective_system_prompt(
                &model,
                &context,
                &StreamOptions {
                    thinking: ThinkingLevel::Off,
                    ..StreamOptions::default()
                },
            )
            .expect("off prompt");
        let low = provider
            .effective_system_prompt(
                &model,
                &context,
                &StreamOptions {
                    thinking: ThinkingLevel::Low,
                    ..StreamOptions::default()
                },
            )
            .expect("low prompt");
        let medium = provider
            .effective_system_prompt(
                &model,
                &context,
                &StreamOptions {
                    thinking: ThinkingLevel::Medium,
                    ..StreamOptions::default()
                },
            )
            .expect("medium prompt");
        let high = provider
            .effective_system_prompt(
                &model,
                &context,
                &StreamOptions {
                    thinking: ThinkingLevel::High,
                    ..StreamOptions::default()
                },
            )
            .expect("high prompt");

        assert!(off.contains("answer directly"));
        assert!(low.contains("brief internal plan"));
        assert!(medium.contains("balanced internal planning"));
        assert!(high.contains("reason deliberately"));
        assert_ne!(off, low);
        assert_ne!(low, medium);
        assert_ne!(medium, high);
    }

    #[test]
    fn local_request_body_adds_prompt_steering_without_native_reasoning_fields() {
        let provider = OpenAIProvider::new();
        let model = sample_local_model();
        let body = provider.build_request_body(
            &model,
            &LlmContext {
                system_prompt: "Follow the user's instructions carefully.".into(),
                messages: vec![LlmMessage {
                    role: "user".into(),
                    content: json!("hello"),
                }],
                tools: vec![ToolDef {
                    name: "read".into(),
                    description: "Read a file".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        }
                    }),
                }],
            },
            &StreamOptions {
                thinking: ThinkingLevel::High,
                ..StreamOptions::default()
            },
            true,
        );

        let system_prompt = body["messages"][0]["content"]
            .as_str()
            .expect("system prompt text");
        assert!(system_prompt.starts_with("Follow the user's instructions carefully."));
        assert!(system_prompt.contains("reason deliberately"));
        assert_eq!(
            body["messages"][1],
            json!({ "role": "user", "content": "hello" })
        );
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["tools"][0]["function"]["name"], json!("read"));
    }

    #[test]
    fn hosted_blank_system_prompt_stays_omitted_without_local_prompt_steering() {
        let provider = OpenAIProvider::new();
        let model = sample_model();
        let body = provider.build_request_body(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![LlmMessage {
                    role: "user".into(),
                    content: json!("hello"),
                }],
                tools: vec![],
            },
            &StreamOptions {
                thinking: ThinkingLevel::High,
                ..StreamOptions::default()
            },
            true,
        );

        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            body["messages"][0],
            json!({ "role": "user", "content": "hello" })
        );
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(body["reasoning"], json!({ "summary": "auto" }));
    }

    #[test]
    fn ollama_native_reasoning_profile_emits_top_level_reasoning_effort() {
        let provider = OpenAIProvider::new();
        let model = sample_native_local_model(
            "ollama",
            "http://localhost:11434/v1",
            ThinkingRequestMode::ReasoningEffort,
        );
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![LlmMessage {
                    role: "user".into(),
                    content: json!("hello"),
                }],
                tools: vec![],
            },
            &options,
            true,
            strategies[0],
        );

        assert_eq!(
            strategies,
            vec![
                NativeReasoningRequestStrategy::TopLevelReasoningEffort,
                NativeReasoningRequestStrategy::NoNativeFields,
            ]
        );
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert!(body.get("reasoning").is_none());
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .expect("system prompt")
                .contains("reason deliberately")
        );
    }

    #[test]
    fn vllm_native_reasoning_profile_emits_top_level_reasoning_effort() {
        let provider = OpenAIProvider::new();
        let model = sample_native_local_model(
            "vllm",
            "http://localhost:8000/v1",
            ThinkingRequestMode::ReasoningEffort,
        );
        let options = StreamOptions {
            thinking: ThinkingLevel::Medium,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            strategies[0],
        );

        assert_eq!(
            strategies,
            vec![
                NativeReasoningRequestStrategy::TopLevelReasoningEffort,
                NativeReasoningRequestStrategy::NoNativeFields,
            ]
        );
        assert_eq!(body["reasoning_effort"], json!("medium"));
    }

    #[test]
    fn lmstudio_native_reasoning_profile_uses_nested_reasoning_effort() {
        let provider = OpenAIProvider::new();
        let model = sample_native_local_model(
            "lmstudio",
            "http://localhost:1234/v1",
            ThinkingRequestMode::NestedReasoning,
        );
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            ..StreamOptions::default()
        };

        assert_eq!(
            provider.native_reasoning_request_strategies(&model, &options),
            vec![
                NativeReasoningRequestStrategy::NestedReasoning,
                NativeReasoningRequestStrategy::NoNativeFields,
            ]
        );

        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            NativeReasoningRequestStrategy::NestedReasoning,
        );
        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn thinking_off_does_not_force_native_reasoning_fields_for_local_native_models() {
        let provider = OpenAIProvider::new();
        let model = sample_native_local_model(
            "ollama",
            "http://localhost:11434/v1",
            ThinkingRequestMode::ReasoningEffort,
        );
        let options = StreamOptions {
            thinking: ThinkingLevel::Off,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            strategies[0],
        );

        assert_eq!(
            strategies,
            vec![NativeReasoningRequestStrategy::NoNativeFields]
        );
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn non_thinking_ollama_model_silently_drops_user_thinking_level() {
        // PR 5 invariant. A non-thinking Ollama model (say,
        // `gemma3:1b` with `/api/show` capabilities =
        // `["completion"]`) surfaces as a `Model` with
        // `reasoning_capabilities = None` and
        // `supports_reasoning = false`. The user's thinking
        // level (`ThinkingLevel::Low` below) must be silently
        // dropped — no reasoning_effort, no reasoning block, no
        // enable_thinking, no chat_template_kwargs, no error,
        // no warning.
        let provider = OpenAIProvider::new();
        let model = Model {
            id: "gemma3:1b".into(),
            name: "gemma3:1b".into(),
            provider: "ollama".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost:11434/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        };
        let options = StreamOptions {
            thinking: ThinkingLevel::Low,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            strategies[0],
        );

        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
    }

    #[test]
    fn non_thinking_hosted_model_silently_drops_user_thinking_level() {
        // Sibling invariant for hosted (non-local) models. A
        // refactor that condition-gated solely on
        // `is_local_openai_compatible_target` would let this case
        // through; guarded here so the silent-drop invariant
        // holds for both local and hosted non-thinking models.
        let provider = OpenAIProvider::new();
        let model = Model {
            id: "gpt-3.5-turbo".into(),
            name: "GPT-3.5 Turbo".into(),
            provider: "openai".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".into(),
            context_window: 16_385,
            max_tokens: 4_096,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        };
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            strategies[0],
        );

        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("enable_thinking").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
    }

    /// Hosted model that declares `NestedReasoning` — e.g. an
    /// OpenRouter catalog entry routed to an upstream that
    /// normalizes via the nested `reasoning` object.
    fn sample_nested_hosted_model() -> Model {
        Model {
            id: "anthropic/claude-sonnet-4".into(),
            name: "Claude Sonnet 4 (OpenRouter)".into(),
            provider: "openrouter".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://openrouter.ai/api/v1".into(),
            context_window: 200_000,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::NestedReasoning),
            }),
            supports_images: true,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn nested_body_for(model: &Model, thinking: ThinkingLevel) -> serde_json::Value {
        let provider = OpenAIProvider::new();
        let options = StreamOptions {
            thinking,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(model, &options);
        assert_eq!(
            strategies[0],
            NativeReasoningRequestStrategy::NestedReasoning,
            "nested strategy should lead for declared NestedReasoning models"
        );
        provider.build_request_body_with_native_reasoning_strategy(
            model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            NativeReasoningRequestStrategy::NestedReasoning,
        )
    }

    #[test]
    fn nested_reasoning_emits_reasoning_object_with_effort() {
        let model = sample_nested_hosted_model();
        let body = nested_body_for(&model, ThinkingLevel::High);
        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn nested_reasoning_off_level_emits_effort_none() {
        let model = sample_nested_hosted_model();
        let body = nested_body_for(&model, ThinkingLevel::Off);
        assert_eq!(body["reasoning"], json!({ "effort": "none" }));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn nested_reasoning_maps_every_thinking_level() {
        let model = sample_nested_hosted_model();
        for (level, expected) in [
            (ThinkingLevel::Off, "none"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
        ] {
            let body = nested_body_for(&model, level);
            assert_eq!(
                body["reasoning"],
                json!({ "effort": expected }),
                "thinking level {level:?} should map to effort {expected}"
            );
        }
    }

    fn sample_qwen_enable_thinking_model(nested: bool) -> Model {
        // A vLLM / SGLang-style Qwen3 catalog entry: native
        // thinking control via the `enable_thinking` boolean
        // (`nested = false`) or via `chat_template_kwargs`
        // (`nested = true`).
        Model {
            id: "qwen3-32b".into(),
            name: "Qwen 3 32B".into(),
            provider: "vllm".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://vllm.example.com/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(if nested {
                    ThinkingRequestMode::ChatTemplateEnableThinking
                } else {
                    ThinkingRequestMode::EnableThinkingFlag
                }),
            }),
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn enable_thinking_body_for(
        model: &Model,
        thinking: ThinkingLevel,
        nested: bool,
    ) -> serde_json::Value {
        let provider = OpenAIProvider::new();
        let options = StreamOptions {
            thinking,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(model, &options);
        assert_eq!(
            strategies[0],
            NativeReasoningRequestStrategy::EnableThinkingFlag { nested },
            "EnableThinkingFlag strategy should lead for declared enable-thinking models"
        );
        provider.build_request_body_with_native_reasoning_strategy(
            model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![],
                tools: vec![],
            },
            &options,
            true,
            NativeReasoningRequestStrategy::EnableThinkingFlag { nested },
        )
    }

    #[test]
    fn qwen_enable_thinking_flag_emits_top_level_boolean() {
        let model = sample_qwen_enable_thinking_model(false);
        // Off → false (the whole point of this mode: the user's
        // Off preference actually disables thinking on the wire).
        let off = enable_thinking_body_for(&model, ThinkingLevel::Off, false);
        assert_eq!(off["enable_thinking"], json!(false));
        assert!(off.get("reasoning_effort").is_none());
        assert!(off.get("reasoning").is_none());
        assert!(off.get("chat_template_kwargs").is_none());

        for level in [
            ThinkingLevel::Minimal,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ] {
            let body = enable_thinking_body_for(&model, level, false);
            assert_eq!(
                body["enable_thinking"],
                json!(true),
                "level {level:?} should map to true"
            );
        }
    }

    #[test]
    fn qwen_chat_template_enable_thinking_emits_nested_boolean() {
        let model = sample_qwen_enable_thinking_model(true);
        let off = enable_thinking_body_for(&model, ThinkingLevel::Off, true);
        assert_eq!(
            off["chat_template_kwargs"],
            json!({ "enable_thinking": false })
        );
        assert!(off.get("enable_thinking").is_none());
        assert!(off.get("reasoning_effort").is_none());

        let high = enable_thinking_body_for(&model, ThinkingLevel::High, true);
        assert_eq!(
            high["chat_template_kwargs"],
            json!({ "enable_thinking": true })
        );
    }

    #[test]
    fn enable_thinking_flag_falls_back_to_no_native_fields_on_400() {
        // Same retry-with-fallback semantics as other native
        // strategies: when the primary strategy fails with a
        // reasoning-compat 400, the outer send loop moves on to
        // the NoNativeFields fallback.
        let provider = OpenAIProvider::new();
        let model = sample_qwen_enable_thinking_model(false);
        let options = StreamOptions {
            thinking: ThinkingLevel::Medium,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        assert_eq!(
            strategies,
            vec![
                NativeReasoningRequestStrategy::EnableThinkingFlag { nested: false },
                NativeReasoningRequestStrategy::NoNativeFields,
            ],
        );

        let model_nested = sample_qwen_enable_thinking_model(true);
        let strategies_nested =
            provider.native_reasoning_request_strategies(&model_nested, &options);
        assert_eq!(
            strategies_nested,
            vec![
                NativeReasoningRequestStrategy::EnableThinkingFlag { nested: true },
                NativeReasoningRequestStrategy::NoNativeFields,
            ],
        );
    }

    #[test]
    fn existing_thinking_request_modes_behave_unchanged() {
        // Forward-compat guardrail: adding
        // EnableThinkingFlag / ChatTemplateEnableThinking must
        // not perturb the routing for the three pre-existing
        // variants.
        let provider = OpenAIProvider::new();
        let options = StreamOptions {
            thinking: ThinkingLevel::Medium,
            ..StreamOptions::default()
        };

        // ReasoningEffort → [TopLevelReasoningEffort, NoNativeFields]
        let ollama =
            sample_heuristic_local_model("ollama", "http://localhost:11434/v1", "qwen3:32b");
        assert_eq!(
            provider.native_reasoning_request_strategies(&ollama, &options),
            vec![
                NativeReasoningRequestStrategy::TopLevelReasoningEffort,
                NativeReasoningRequestStrategy::NoNativeFields,
            ],
        );

        // NestedReasoning → [NestedReasoning, NoNativeFields]
        let nested = sample_nested_hosted_model();
        assert_eq!(
            provider.native_reasoning_request_strategies(&nested, &options),
            vec![
                NativeReasoningRequestStrategy::NestedReasoning,
                NativeReasoningRequestStrategy::NoNativeFields,
            ],
        );

        // Hosted reasoning model (no request_mode, supports_reasoning)
        // → [NoNativeFields] (covered already by
        // existing_reasoning_effort_mode_unchanged_for_hosted_reasoning_models,
        // asserted here for completeness).
        let hosted = sample_model();
        assert_eq!(
            provider.native_reasoning_request_strategies(&hosted, &options),
            vec![NativeReasoningRequestStrategy::NoNativeFields],
        );
    }

    #[test]
    fn existing_reasoning_effort_mode_unchanged_for_hosted_reasoning_models() {
        let provider = OpenAIProvider::new();
        let model = sample_model();
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            ..StreamOptions::default()
        };
        let strategies = provider.native_reasoning_request_strategies(&model, &options);
        let body = provider.build_request_body_with_native_reasoning_strategy(
            &model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![LlmMessage {
                    role: "user".into(),
                    content: json!("hello"),
                }],
                tools: vec![],
            },
            &options,
            true,
            strategies[0],
        );

        assert_eq!(
            strategies,
            vec![NativeReasoningRequestStrategy::NoNativeFields]
        );
        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(body["reasoning"], json!({ "summary": "auto" }));
    }

    /// Build a minimal OpenRouter-targeted model with the given
    /// upstream-prefixed id. Reasoning is left off so tests can
    /// layer it via the compat blob / capabilities field.
    fn openrouter_model(id: &str) -> Model {
        let mut model = sample_nested_hosted_model();
        model.id = id.into();
        model.provider = "openrouter".into();
        model.base_url = "https://openrouter.ai/api/v1".into();
        model
    }

    fn openai_request_body_for(model: &Model, user_text: &str) -> serde_json::Value {
        let provider = OpenAIProvider::new();
        let context = LlmContext {
            system_prompt: String::new(),
            messages: vec![LlmMessage {
                role: "user".into(),
                content: json!(user_text),
            }],
            tools: Vec::new(),
        };
        provider.build_request_body(model, &context, &StreamOptions::default(), true)
    }

    #[test]
    fn openrouter_anthropic_upstream_adds_cache_control_to_last_text() {
        let model = openrouter_model("anthropic/claude-sonnet-4");
        let body = openai_request_body_for(&model, "describe this");

        let messages = body["messages"].as_array().expect("messages array");
        let last = messages.last().expect("last message");
        let last_part = last["content"]
            .as_array()
            .expect("content array")
            .last()
            .expect("last content part")
            .clone();
        assert_eq!(last_part["type"], "text");
        assert_eq!(last_part["text"], "describe this");
        assert_eq!(last_part["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn openrouter_non_anthropic_upstream_does_not_add_cache_control() {
        let model = openrouter_model("openai/o3");
        let body = openai_request_body_for(&model, "hi");

        let serialized = serde_json::to_string(&body).expect("serialize body");
        assert!(
            !serialized.contains("cache_control"),
            "cache_control must not appear for non-anthropic upstreams: {serialized}"
        );
    }

    #[test]
    fn openrouter_routing_preferences_propagate_to_body() {
        use anie_provider::{OpenAICompletionsCompat, OpenRouterRouting};
        let mut model = openrouter_model("anthropic/claude-sonnet-4");
        model.compat = ModelCompat::OpenAICompletions(OpenAICompletionsCompat {
            openrouter_routing: Some(OpenRouterRouting {
                allow_fallbacks: Some(true),
                order: Some(vec!["anthropic".into()]),
                only: None,
                ignore: None,
                zdr: Some(true),
            }),
            ..Default::default()
        });
        let body = openai_request_body_for(&model, "hi");
        let provider_field = body["provider"].as_object().expect("provider object");
        assert_eq!(provider_field["allow_fallbacks"], json!(true));
        assert_eq!(provider_field["zdr"], json!(true));
        assert_eq!(provider_field["order"], json!(["anthropic"]));
        assert!(!provider_field.contains_key("ignore"));
        assert!(!provider_field.contains_key("only"));
    }

    #[test]
    fn openrouter_routing_none_omits_provider_field() {
        // Default compat (no routing) must not emit a `provider`
        // field — OpenRouter treats its absence as "use the usual
        // routing heuristics", which is what we want by default.
        let model = openrouter_model("anthropic/claude-sonnet-4");
        let body = openai_request_body_for(&model, "hi");
        assert!(!body.as_object().expect("body").contains_key("provider"));
    }

    #[test]
    fn openrouter_routing_empty_struct_omits_provider_field() {
        // All-None `OpenRouterRouting` serializes to `{}`. We
        // explicitly skip emitting the `provider` field in that
        // case so we don't confuse OpenRouter with an empty object.
        use anie_provider::{OpenAICompletionsCompat, OpenRouterRouting};
        let mut model = openrouter_model("openai/o3");
        model.compat = ModelCompat::OpenAICompletions(OpenAICompletionsCompat {
            openrouter_routing: Some(OpenRouterRouting::default()),
            ..Default::default()
        });
        let body = openai_request_body_for(&model, "hi");
        assert!(!body.as_object().expect("body").contains_key("provider"));
    }

    // ---------------------------------------------------------
    // Plan 01 PR A — max_tokens_field compat flag.
    // ---------------------------------------------------------

    fn nemotron_style_hosted_model() -> Model {
        // Hosted reasoning model without the MaxCompletionTokens
        // compat flag — should emit the legacy `max_tokens` field.
        let mut model = sample_nested_hosted_model();
        model.compat = ModelCompat::None;
        model
    }

    fn openai_request_body_with_tokens(model: &Model) -> serde_json::Value {
        let provider = OpenAIProvider::new();
        let options = StreamOptions {
            max_tokens: Some(2_048),
            ..StreamOptions::default()
        };
        provider.build_request_body(
            model,
            &LlmContext {
                system_prompt: String::new(),
                messages: vec![LlmMessage {
                    role: "user".into(),
                    content: json!("hi"),
                }],
                tools: Vec::new(),
            },
            &options,
            true,
        )
    }

    #[test]
    fn max_tokens_field_defaults_to_legacy_max_tokens() {
        let body = openai_request_body_with_tokens(&nemotron_style_hosted_model());
        assert_eq!(body["max_tokens"], json!(2_048));
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn max_tokens_field_max_tokens_explicit_matches_legacy_default() {
        use anie_provider::{MaxTokensField, OpenAICompletionsCompat};
        let mut model = nemotron_style_hosted_model();
        model.compat = ModelCompat::OpenAICompletions(OpenAICompletionsCompat {
            max_tokens_field: Some(MaxTokensField::MaxTokens),
            ..Default::default()
        });
        let body = openai_request_body_with_tokens(&model);
        assert_eq!(body["max_tokens"], json!(2_048));
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn max_tokens_field_max_completion_tokens_emits_new_name() {
        // Regression for OpenAI o-series / GPT-5: the legacy
        // `max_tokens` name 400s on these endpoints. When the
        // catalog opts in, the new field name must appear
        // instead.
        use anie_provider::{MaxTokensField, OpenAICompletionsCompat};
        let mut model = nemotron_style_hosted_model();
        model.compat = ModelCompat::OpenAICompletions(OpenAICompletionsCompat {
            max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
            ..Default::default()
        });
        let body = openai_request_body_with_tokens(&model);
        assert_eq!(body["max_completion_tokens"], json!(2_048));
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn builtin_o4_mini_opts_into_max_completion_tokens() {
        let catalog = crate::builtin_models();
        let o4_mini = catalog
            .iter()
            .find(|model| model.id == "o4-mini")
            .expect("o4-mini");
        match &o4_mini.compat {
            ModelCompat::OpenAICompletions(compat) => {
                assert_eq!(
                    compat.max_tokens_field,
                    Some(anie_provider::MaxTokensField::MaxCompletionTokens),
                );
            }
            other => panic!("expected OpenAICompletions compat, got {other:?}"),
        }
    }
}
