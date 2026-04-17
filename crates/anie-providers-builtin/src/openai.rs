use std::collections::BTreeMap;

use async_stream::try_stream;
use futures::StreamExt;
use serde_json::json;

use anie_protocol::{
    AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolDef, Usage, now_millis,
};
use anie_provider::{
    ApiKind, LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderEvent, ProviderStream,
    ReasoningCapabilities, ReasoningControlMode, StreamOptions, ThinkingLevel,
    ThinkingRequestMode,
};

use crate::{
    classify_http_error, create_http_client, local::default_local_reasoning_capabilities,
    parse_retry_after, sse_stream,
};

/// OpenAI-compatible chat-completions provider implementation.
#[derive(Clone)]
pub struct OpenAIProvider {
    client: reqwest::Client,
}

impl OpenAIProvider {
    /// Create a new provider with the shared HTTP client configuration.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: create_http_client(),
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
        body.insert(
            "messages".into(),
            serde_json::Value::Array(self.openai_request_messages(model, context, options)),
        );

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
            body.insert("max_tokens".into(), json!(max_tokens));
        }
        if let Some(reasoning_effort) = reasoning_effort(options.thinking) {
            match native_reasoning_strategy {
                NativeReasoningRequestStrategy::TopLevelReasoningEffort => {
                    body.insert("reasoning_effort".into(), json!(reasoning_effort));
                }
                NativeReasoningRequestStrategy::LmStudioNestedReasoning => {
                    body.insert("reasoning".into(), json!({ "effort": reasoning_effort }));
                }
                NativeReasoningRequestStrategy::NoNativeFields
                    if model.supports_reasoning && !is_local_openai_compatible_target(model) =>
                {
                    body.insert("reasoning_effort".into(), json!(reasoning_effort));
                    body.insert("reasoning".into(), json!({ "summary": "auto" }));
                }
                NativeReasoningRequestStrategy::NoNativeFields => {}
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
            .map_err(|error| ProviderError::Request(error.to_string()))
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
            return Err(classify_http_error(status, &body, retry_after));
        }

        Err(classify_http_error(status, &body, retry_after))
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

        Err(ProviderError::Request(
            "no compatible OpenAI request strategy available".into(),
        ))
    }

    fn native_reasoning_request_strategies(
        &self,
        model: &Model,
        options: &StreamOptions,
    ) -> Vec<NativeReasoningRequestStrategy> {
        if options.thinking == ThinkingLevel::Off || !is_local_openai_compatible_target(model) {
            return vec![NativeReasoningRequestStrategy::NoNativeFields];
        }

        let Some(capabilities) = effective_reasoning_capabilities(model) else {
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
                Some(NativeReasoningRequestStrategy::LmStudioNestedReasoning)
            }
            Some(ThinkingRequestMode::PromptSteering) => None,
            None => match openai_compatible_backend(model) {
                OpenAiCompatibleBackend::LmStudio => {
                    Some(NativeReasoningRequestStrategy::LmStudioNestedReasoning)
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
                let event = event.map_err(|error| ProviderError::Stream(error.to_string()))?;
                for provider_event in state.process_event(&event.data)? {
                    yield provider_event;
                }
            }

            if !state.finished {
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
                    content: serde_json::Value::String(join_text_content(&user_message.content)),
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

fn assistant_message_to_openai_llm_message(
    assistant_message: &AssistantMessage,
) -> Option<LlmMessage> {
    let text = assistant_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
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

    Some(LlmMessage {
        role: "assistant".into(),
        content: serde_json::Value::Object(payload),
    })
}

fn llm_message_to_openai_message(message: &LlmMessage) -> serde_json::Value {
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

fn join_text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::Thinking { thinking } => Some(thinking.clone()),
            ContentBlock::Image { media_type, data } => {
                Some(format!("[image:{media_type};base64,{data}]"))
            }
            ContentBlock::ToolCall(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn reasoning_effort(thinking: ThinkingLevel) -> Option<&'static str> {
    match thinking {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
    }
}

fn is_local_openai_compatible_target(model: &Model) -> bool {
    if model.api != ApiKind::OpenAICompletions {
        return false;
    }

    if matches!(model.provider.as_str(), "ollama" | "lmstudio") {
        return true;
    }

    let base_url = model.base_url.trim().to_ascii_lowercase();
    [
        "http://localhost",
        "https://localhost",
        "http://127.0.0.1",
        "https://127.0.0.1",
        "http://[::1]",
        "https://[::1]",
    ]
    .iter()
    .any(|prefix| base_url.starts_with(prefix))
}

fn local_reasoning_prompt_steering(thinking: ThinkingLevel) -> &'static str {
    match thinking {
        ThinkingLevel::Off => {
            "For this response, answer directly and avoid a visible reasoning block unless it is necessary."
        }
        ThinkingLevel::Low => {
            "For this response, do a brief internal plan and keep reasoning concise before answering."
        }
        ThinkingLevel::Medium => {
            "For this response, do balanced internal planning and check key assumptions before answering."
        }
        ThinkingLevel::High => {
            "For this response, reason deliberately and verify the answer before finalizing it."
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum NativeReasoningRequestStrategy {
    NoNativeFields,
    TopLevelReasoningEffort,
    LmStudioNestedReasoning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiCompatibleBackend {
    Hosted,
    Ollama,
    LmStudio,
    Vllm,
    UnknownLocal,
}

fn effective_reasoning_capabilities(model: &Model) -> Option<ReasoningCapabilities> {
    model.reasoning_capabilities.clone().or_else(|| {
        default_local_reasoning_capabilities(&model.provider, &model.base_url, &model.id)
    })
}

fn effective_max_tokens(model: &Model, options: &StreamOptions) -> Option<u64> {
    let max_tokens = options.max_tokens?;
    if !is_local_openai_compatible_target(model) {
        return Some(max_tokens);
    }

    let visible_reasoning_output_likely = effective_reasoning_capabilities(model)
        .as_ref()
        .and_then(|capabilities| capabilities.output)
        .is_some();
    let base_headroom = match options.thinking {
        ThinkingLevel::Off => 0,
        ThinkingLevel::Low => max_tokens / 10,
        ThinkingLevel::Medium => max_tokens / 5,
        ThinkingLevel::High => max_tokens / 4,
    };
    let visible_reasoning_headroom = if visible_reasoning_output_likely {
        match options.thinking {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Low => 128,
            ThinkingLevel::Medium => 256,
            ThinkingLevel::High => 512,
        }
    } else {
        0
    };
    let headroom = base_headroom
        .saturating_add(visible_reasoning_headroom)
        .min(max_tokens.saturating_sub(1));

    Some(max_tokens.saturating_sub(headroom).max(1))
}

fn openai_compatible_backend(model: &Model) -> OpenAiCompatibleBackend {
    if !is_local_openai_compatible_target(model) {
        return OpenAiCompatibleBackend::Hosted;
    }

    if model.provider == "ollama" || model.base_url.contains(":11434") {
        return OpenAiCompatibleBackend::Ollama;
    }
    if model.provider == "lmstudio" || model.base_url.contains(":1234") {
        return OpenAiCompatibleBackend::LmStudio;
    }
    if model.provider.eq_ignore_ascii_case("vllm") {
        return OpenAiCompatibleBackend::Vllm;
    }

    OpenAiCompatibleBackend::UnknownLocal
}

fn is_native_reasoning_compatibility_error(error: &ProviderError) -> bool {
    let ProviderError::Http { status, body } = error else {
        return false;
    };
    if *status != 400 {
        return false;
    }

    let body = body.to_ascii_lowercase();
    let mentions_reasoning_field = body.contains("reasoning_effort")
        || body.contains("reasoning.effort")
        || body.contains("\"reasoning\"")
        || body.contains("'reasoning'")
        || body.contains("reasoning")
        || body.contains(" field required") && body.contains("reasoning");
    let indicates_compatibility_failure = body.contains("unknown")
        || body.contains("unsupported")
        || body.contains("unexpected")
        || body.contains("unrecognized")
        || body.contains("extra inputs")
        || body.contains("not permitted")
        || body.contains("additional properties")
        || body.contains("invalid")
        || body.contains("bad request");

    mentions_reasoning_field && indicates_compatibility_failure
}

fn native_reasoning_delta(delta: &serde_json::Value) -> Option<String> {
    ["reasoning", "reasoning_content", "thinking"]
        .iter()
        .find_map(|field| {
            delta
                .get(*field)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

const TAGGED_REASONING_TAGS: [(&str, &str); 3] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];

enum StreamContentPart {
    Text(String),
    Thinking(String),
}

#[derive(Clone, Copy)]
enum TaggedReasoningMode {
    Text,
    Thinking { closing_tag: &'static str },
}

struct TaggedReasoningSplitter {
    mode: TaggedReasoningMode,
    pending: String,
}

impl Default for TaggedReasoningSplitter {
    fn default() -> Self {
        Self {
            mode: TaggedReasoningMode::Text,
            pending: String::new(),
        }
    }
}

impl TaggedReasoningSplitter {
    fn push(&mut self, fragment: &str) -> Vec<StreamContentPart> {
        self.pending.push_str(fragment);
        self.drain(false)
    }

    fn finish(&mut self) -> Vec<StreamContentPart> {
        self.drain(true)
    }

    fn drain(&mut self, finish: bool) -> Vec<StreamContentPart> {
        let mut parts = Vec::new();

        loop {
            match self.mode {
                TaggedReasoningMode::Text => {
                    if self.pending.is_empty() {
                        break;
                    }

                    if let Some(open_index) = self.pending.find('<') {
                        if open_index > 0 {
                            let text = self.pending.drain(..open_index).collect::<String>();
                            Self::push_part(&mut parts, StreamContentPart::Text(text));
                            continue;
                        }

                        if let Some((open_tag, closing_tag)) =
                            tagged_reasoning_open_tag(&self.pending)
                        {
                            self.pending.drain(..open_tag.len());
                            self.mode = TaggedReasoningMode::Thinking { closing_tag };
                            continue;
                        }

                        if !finish && is_prefix_of_any_open_tag(&self.pending) {
                            break;
                        }

                        let text = drain_first_char(&mut self.pending);
                        Self::push_part(&mut parts, StreamContentPart::Text(text));
                        continue;
                    }

                    let text = std::mem::take(&mut self.pending);
                    Self::push_part(&mut parts, StreamContentPart::Text(text));
                    break;
                }
                TaggedReasoningMode::Thinking { closing_tag } => {
                    if self.pending.is_empty() {
                        break;
                    }

                    if let Some(close_index) = self.pending.find('<') {
                        if close_index > 0 {
                            let thinking = self.pending.drain(..close_index).collect::<String>();
                            Self::push_part(&mut parts, StreamContentPart::Thinking(thinking));
                            continue;
                        }

                        if self.pending.starts_with(closing_tag) {
                            self.pending.drain(..closing_tag.len());
                            self.mode = TaggedReasoningMode::Text;
                            continue;
                        }

                        if !finish && closing_tag.starts_with(&self.pending) {
                            break;
                        }

                        let thinking = drain_first_char(&mut self.pending);
                        Self::push_part(&mut parts, StreamContentPart::Thinking(thinking));
                        continue;
                    }

                    let thinking = std::mem::take(&mut self.pending);
                    Self::push_part(&mut parts, StreamContentPart::Thinking(thinking));
                    break;
                }
            }
        }

        parts
    }

    fn push_part(parts: &mut Vec<StreamContentPart>, part: StreamContentPart) {
        match part {
            StreamContentPart::Text(text) if text.is_empty() => {}
            StreamContentPart::Thinking(thinking) if thinking.is_empty() => {}
            StreamContentPart::Text(text) => match parts.last_mut() {
                Some(StreamContentPart::Text(existing)) => existing.push_str(&text),
                _ => parts.push(StreamContentPart::Text(text)),
            },
            StreamContentPart::Thinking(thinking) => match parts.last_mut() {
                Some(StreamContentPart::Thinking(existing)) => existing.push_str(&thinking),
                _ => parts.push(StreamContentPart::Thinking(thinking)),
            },
        }
    }
}

fn tagged_reasoning_open_tag(input: &str) -> Option<(&'static str, &'static str)> {
    TAGGED_REASONING_TAGS
        .iter()
        .find_map(|(open_tag, closing_tag)| {
            input
                .starts_with(open_tag)
                .then_some((*open_tag, *closing_tag))
        })
}

fn is_prefix_of_any_open_tag(input: &str) -> bool {
    TAGGED_REASONING_TAGS
        .iter()
        .any(|(open_tag, _)| open_tag.starts_with(input))
}

fn drain_first_char(input: &mut String) -> String {
    let first_char_len = input.chars().next().map_or(0, char::len_utf8);
    input.drain(..first_char_len).collect()
}

struct OpenAiStreamState {
    model: Model,
    text: String,
    thinking: String,
    tagged_reasoning: TaggedReasoningSplitter,
    tool_calls: BTreeMap<usize, OpenAiToolCallState>,
    usage: Usage,
    finish_reason: Option<String>,
    finished: bool,
}

impl OpenAiStreamState {
    fn new(model: &Model) -> Self {
        Self {
            model: model.clone(),
            text: String::new(),
            thinking: String::new(),
            tagged_reasoning: TaggedReasoningSplitter::default(),
            tool_calls: BTreeMap::new(),
            usage: Usage::default(),
            finish_reason: None,
            finished: false,
        }
    }

    fn process_event(&mut self, data: &str) -> Result<Vec<ProviderEvent>, ProviderError> {
        if data == "[DONE]" {
            return self.finish_stream();
        }

        let payload: serde_json::Value =
            serde_json::from_str(data).map_err(|error| ProviderError::Stream(error.to_string()))?;
        let mut events = Vec::new();

        if let Some(usage) = payload.get("usage") {
            self.usage.input_tokens = usage["prompt_tokens"].as_u64().unwrap_or(0);
            self.usage.output_tokens = usage["completion_tokens"].as_u64().unwrap_or(0);
            self.usage.total_tokens = usage["total_tokens"].as_u64();
        }

        if let Some(choices) = payload.get("choices").and_then(serde_json::Value::as_array) {
            for choice in choices {
                let Some(delta) = choice.get("delta") else {
                    if let Some(finish_reason) = choice
                        .get("finish_reason")
                        .and_then(serde_json::Value::as_str)
                        && !finish_reason.is_empty()
                    {
                        self.finish_reason = Some(finish_reason.to_string());
                        if finish_reason == "tool_calls" {
                            events.extend(self.finish_tool_calls());
                        }
                    }
                    continue;
                };

                let has_native_reasoning = if let Some(reasoning) = native_reasoning_delta(delta) {
                    self.thinking.push_str(&reasoning);
                    events.push(ProviderEvent::ThinkingDelta(reasoning));
                    true
                } else {
                    false
                };

                if let Some(content) = delta.get("content").and_then(serde_json::Value::as_str) {
                    if has_native_reasoning {
                        if !content.is_empty() {
                            self.text.push_str(content);
                            events.push(ProviderEvent::TextDelta(content.to_string()));
                        }
                    } else {
                        let parts = self.tagged_reasoning.push(content);
                        self.push_stream_content_parts(parts, &mut events);
                    }
                }

                if let Some(tool_calls) = delta
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                {
                    for tool_call in tool_calls {
                        let index = tool_call
                            .get("index")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0) as usize;
                        let state = self
                            .tool_calls
                            .entry(index)
                            .or_insert_with(OpenAiToolCallState::new);
                        if let Some(id) = tool_call.get("id").and_then(serde_json::Value::as_str) {
                            state.id = id.to_string();
                        }
                        if let Some(name) = tool_call
                            .get("function")
                            .and_then(|function| function.get("name"))
                            .and_then(serde_json::Value::as_str)
                        {
                            state.name = name.to_string();
                        }
                        if !state.started && !state.id.is_empty() && !state.name.is_empty() {
                            state.started = true;
                            events.push(ProviderEvent::ToolCallStart(ToolCall {
                                id: state.id.clone(),
                                name: state.name.clone(),
                                arguments: serde_json::Value::Null,
                            }));
                        }
                        if let Some(arguments_delta) = tool_call
                            .get("function")
                            .and_then(|function| function.get("arguments"))
                            .and_then(serde_json::Value::as_str)
                            && !arguments_delta.is_empty()
                        {
                            state.arguments.push_str(arguments_delta);
                            events.push(ProviderEvent::ToolCallDelta {
                                id: state.id.clone(),
                                arguments_delta: arguments_delta.to_string(),
                            });
                        }
                    }
                }

                if let Some(finish_reason) = choice
                    .get("finish_reason")
                    .and_then(serde_json::Value::as_str)
                    && !finish_reason.is_empty()
                {
                    self.finish_reason = Some(finish_reason.to_string());
                    if finish_reason == "tool_calls" {
                        events.extend(self.finish_tool_calls());
                    }
                }
            }
        }

        Ok(events)
    }

    fn push_stream_content_parts(
        &mut self,
        parts: Vec<StreamContentPart>,
        events: &mut Vec<ProviderEvent>,
    ) {
        for part in parts {
            match part {
                StreamContentPart::Text(text) => {
                    self.text.push_str(&text);
                    events.push(ProviderEvent::TextDelta(text));
                }
                StreamContentPart::Thinking(thinking) => {
                    self.thinking.push_str(&thinking);
                    events.push(ProviderEvent::ThinkingDelta(thinking));
                }
            }
        }
    }

    fn finish_tagged_content(&mut self) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        let parts = self.tagged_reasoning.finish();
        self.push_stream_content_parts(parts, &mut events);
        events
    }

    fn finish_stream(&mut self) -> Result<Vec<ProviderEvent>, ProviderError> {
        let mut events = self.finish_tagged_content();
        events.extend(self.finish_tool_calls());
        if !self.has_meaningful_content() {
            return Err(ProviderError::Stream("empty assistant response".into()));
        }
        events.push(ProviderEvent::Done(self.into_message()));
        Ok(events)
    }

    fn finish_tool_calls(&mut self) -> Vec<ProviderEvent> {
        let mut events = Vec::new();
        for state in self.tool_calls.values_mut() {
            if !state.ended && !state.id.is_empty() {
                state.ended = true;
                events.push(ProviderEvent::ToolCallEnd {
                    id: state.id.clone(),
                });
            }
        }
        events
    }

    fn has_meaningful_content(&self) -> bool {
        !self.text.is_empty() || self.tool_calls.values().any(|state| !state.id.is_empty())
    }

    fn into_message(&mut self) -> AssistantMessage {
        self.finished = true;
        let _ = self.finish_tagged_content();
        let mut content = Vec::new();
        if !self.thinking.is_empty() {
            content.push(ContentBlock::Thinking {
                thinking: std::mem::take(&mut self.thinking),
            });
        }
        if !self.text.is_empty() {
            content.push(ContentBlock::Text {
                text: std::mem::take(&mut self.text),
            });
        }
        for state in self.tool_calls.values_mut() {
            let arguments = match serde_json::from_str(&state.arguments) {
                Ok(arguments) => arguments,
                Err(error) => {
                    json!({
                        "_raw": state.arguments,
                        "_error": error.to_string(),
                    })
                }
            };
            content.push(ContentBlock::ToolCall(ToolCall {
                id: state.id.clone(),
                name: state.name.clone(),
                arguments,
            }));
        }

        AssistantMessage {
            content,
            usage: std::mem::take(&mut self.usage),
            stop_reason: match self.finish_reason.as_deref() {
                Some("tool_calls") => StopReason::ToolUse,
                Some("stop") | Some("length") | None => StopReason::Stop,
                _ => StopReason::Stop,
            },
            error_message: None,
            provider: self.model.provider.clone(),
            model: self.model.id.clone(),
            timestamp: now_millis(),
        }
    }
}

#[derive(Default)]
struct OpenAiToolCallState {
    id: String,
    name: String,
    arguments: String,
    started: bool,
    ended: bool,
}

impl OpenAiToolCallState {
    fn new() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use anie_provider::{
        ApiKind, ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode,
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
        }
    }

    fn assistant_text(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn assistant_thinking(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    fn final_message(events: &[ProviderEvent]) -> AssistantMessage {
        events
            .iter()
            .find_map(|event| match event {
                ProviderEvent::Done(message) => Some(message.clone()),
                _ => None,
            })
            .expect("done event")
    }

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
    fn openai_provider_does_not_replay_thinking_blocks() {
        let provider = OpenAIProvider::new();
        assert!(!provider.includes_thinking_in_replay());
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
    fn accumulates_argument_fragments_into_tool_calls() {
        let mut state = OpenAiStreamState::new(&sample_model());
        let first = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#)
            .expect("first chunk");
        let second = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"src/main.rs\"}"}}],"content":""},"finish_reason":"tool_calls"}]}"#)
            .expect("second chunk");

        assert!(matches!(
            first.first(),
            Some(ProviderEvent::ToolCallStart(_))
        ));
        assert!(matches!(
            second.last(),
            Some(ProviderEvent::ToolCallEnd { .. })
        ));

        let message = state.into_message();
        assert!(matches!(
            &message.content[0],
            ContentBlock::ToolCall(ToolCall { arguments, .. }) if arguments == &json!({ "path": "src/main.rs" })
        ));
    }

    #[test]
    fn handles_missing_usage_fields() {
        let mut state = OpenAiStreamState::new(&sample_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        assert!(matches!(events.first(), Some(ProviderEvent::TextDelta(text)) if text == "hello"));
        let message = state.into_message();
        assert_eq!(message.usage.input_tokens, 0);
        assert_eq!(message.usage.output_tokens, 0);
    }

    #[test]
    fn parses_native_reasoning_fields_into_thinking_deltas() {
        for (field, value) in [
            ("reasoning", "plan"),
            ("reasoning_content", "legacy-plan"),
            ("thinking", "older-plan"),
        ] {
            let mut state = OpenAiStreamState::new(&sample_local_model());
            let events = state
                .process_event(
                    &json!({
                        "choices": [{
                            "index": 0,
                            "delta": { field: value },
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string(),
                )
                .expect("events");
            let message = state.into_message();

            assert_eq!(events, vec![ProviderEvent::ThinkingDelta(value.into())]);
            assert_eq!(assistant_thinking(&message), value);
            assert_eq!(assistant_text(&message), "");
        }
    }

    #[test]
    fn reasoning_only_stream_without_visible_content_is_an_error() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"","reasoning":"hello from reasoning"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let error = state.finish_stream().expect_err("finish stream should fail");

        assert_eq!(
            events,
            vec![ProviderEvent::ThinkingDelta("hello from reasoning".into())]
        );
        assert!(matches!(
            error,
            ProviderError::Stream(message) if message == "empty assistant response"
        ));
    }

    #[test]
    fn reasoning_with_visible_text_still_succeeds() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        state.process_event(
            r#"{"choices":[{"index":0,"delta":{"reasoning":"plan","content":"answer"},"finish_reason":"stop"}]}"#,
        )
        .expect("events");

        let finished = state.finish_stream().expect("finish stream");
        let message = final_message(&finished);

        assert_eq!(assistant_thinking(&message), "plan");
        assert_eq!(assistant_text(&message), "answer");
    }

    #[test]
    fn same_event_native_reasoning_and_text_are_both_preserved() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"reasoning":"plan","content":"answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let message = state.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("plan".into()),
                ProviderEvent::TextDelta("answer".into()),
            ]
        );
        assert_eq!(assistant_thinking(&message), "plan");
        assert_eq!(assistant_text(&message), "answer");
    }

    #[test]
    fn tagged_parsing_remains_a_fallback_when_native_reasoning_fields_are_absent() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"<think>plan</think>answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let message = state.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::ThinkingDelta("plan".into()),
                ProviderEvent::TextDelta("answer".into()),
            ]
        );
        assert_eq!(assistant_thinking(&message), "plan");
        assert_eq!(assistant_text(&message), "answer");
    }

    #[test]
    fn reasoning_only_assistant_messages_are_omitted_from_openai_replay() {
        let provider = OpenAIProvider::new();
        let messages = provider.convert_messages(&[Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Thinking {
                thinking: "plan first".into(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "ollama".into(),
            model: "qwen3:32b".into(),
            timestamp: 1,
        })]);

        assert!(messages.is_empty());
    }

    #[test]
    fn thinking_is_omitted_but_text_and_tools_preserved_in_openai_replay() {
        let provider = OpenAIProvider::new();
        let messages = provider.convert_messages(&[Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "plan first".into(),
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
    fn parses_tagged_reasoning_when_opening_tag_is_split_across_chunks() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let first = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"content":"Before<thi"}}]}"#)
            .expect("first chunk");
        let second = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"nk>plan</think> after"},"finish_reason":"stop"}]}"#,
            )
            .expect("second chunk");
        let finished = state.finish_stream().expect("finish stream");
        let message = final_message(&finished);

        assert_eq!(first, vec![ProviderEvent::TextDelta("Before".into())]);
        assert_eq!(
            second,
            vec![
                ProviderEvent::ThinkingDelta("plan".into()),
                ProviderEvent::TextDelta(" after".into()),
            ]
        );
        assert_eq!(assistant_text(&message), "Before after");
        assert_eq!(assistant_thinking(&message), "plan");
        assert!(!assistant_text(&message).contains("<think>"));
    }

    #[test]
    fn parses_tagged_reasoning_when_closing_tag_is_split_across_chunks() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let first = state
            .process_event(r#"{"choices":[{"index":0,"delta":{"content":"<think>plan</thi"}}]}"#)
            .expect("first chunk");
        let second = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"nk> answer"},"finish_reason":"stop"}]}"#,
            )
            .expect("second chunk");
        let message = state.into_message();

        assert_eq!(first, vec![ProviderEvent::ThinkingDelta("plan".into())]);
        assert_eq!(second, vec![ProviderEvent::TextDelta(" answer".into())]);
        assert_eq!(assistant_text(&message), " answer");
        assert_eq!(assistant_thinking(&message), "plan");
    }

    #[test]
    fn parses_multiple_tagged_reasoning_spans_in_one_response() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"A<think>X</think>B<reasoning>Y</reasoning>C"},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let message = state.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("A".into()),
                ProviderEvent::ThinkingDelta("X".into()),
                ProviderEvent::TextDelta("B".into()),
                ProviderEvent::ThinkingDelta("Y".into()),
                ProviderEvent::TextDelta("C".into()),
            ]
        );
        assert_eq!(assistant_text(&message), "ABC");
        assert_eq!(assistant_thinking(&message), "XY");
    }

    #[test]
    fn tagged_reasoning_aliases_all_emit_thinking() {
        for (content, expected) in [
            ("<think>alpha</think>", "alpha"),
            ("<thinking>beta</thinking>", "beta"),
            ("<reasoning>gamma</reasoning>", "gamma"),
        ] {
            let mut state = OpenAiStreamState::new(&sample_local_model());
            let events = state
                .process_event(
                    &json!({
                        "choices": [{
                            "index": 0,
                            "delta": { "content": content },
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string(),
                )
                .expect("events");
            let message = state.into_message();

            assert_eq!(events, vec![ProviderEvent::ThinkingDelta(expected.into())]);
            assert_eq!(assistant_text(&message), "");
            assert_eq!(assistant_thinking(&message), expected);
        }
    }

    #[test]
    fn malformed_or_unclosed_tag_sequences_do_not_lose_content() {
        let mut malformed = OpenAiStreamState::new(&sample_local_model());
        let first = malformed
            .process_event(r#"{"choices":[{"index":0,"delta":{"content":"hello <thi"}}]}"#)
            .expect("malformed first chunk");
        let finished = malformed.finish_stream().expect("finish stream");
        let malformed_message = final_message(&finished);

        assert_eq!(first, vec![ProviderEvent::TextDelta("hello ".into())]);
        assert!(matches!(
            finished.first(),
            Some(ProviderEvent::TextDelta(text)) if text == "<thi"
        ));
        assert_eq!(assistant_text(&malformed_message), "hello <thi");
        assert_eq!(assistant_thinking(&malformed_message), "");

        let mut unclosed = OpenAiStreamState::new(&sample_local_model());
        let events = unclosed
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":"start<think>plan"},"finish_reason":"stop"}]}"#,
            )
            .expect("unclosed events");
        let unclosed_message = unclosed.into_message();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("start".into()),
                ProviderEvent::ThinkingDelta("plan".into()),
            ]
        );
        assert_eq!(assistant_text(&unclosed_message), "start");
        assert_eq!(assistant_thinking(&unclosed_message), "plan");
    }

    #[test]
    fn truly_empty_successful_stop_becomes_stream_error() {
        let mut state = OpenAiStreamState::new(&sample_local_model());
        let events = state
            .process_event(
                r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}]}"#,
            )
            .expect("events");
        let error = state
            .finish_stream()
            .expect_err("empty response should error");

        assert!(events.is_empty());
        assert!(matches!(
            error,
            ProviderError::Stream(message) if message == "empty assistant response"
        ));
    }

    #[test]
    fn reasoning_effort_maps_from_thinking_level() {
        assert_eq!(reasoning_effort(ThinkingLevel::Off), None);
        assert_eq!(reasoning_effort(ThinkingLevel::Low), Some("low"));
        assert_eq!(reasoning_effort(ThinkingLevel::Medium), Some("medium"));
        assert_eq!(reasoning_effort(ThinkingLevel::High), Some("high"));
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
                NativeReasoningRequestStrategy::LmStudioNestedReasoning,
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
            NativeReasoningRequestStrategy::LmStudioNestedReasoning,
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
    fn native_reasoning_compatibility_errors_are_classified_narrowly() {
        assert!(is_native_reasoning_compatibility_error(
            &ProviderError::Http {
                status: 400,
                body: "unknown field reasoning_effort".into(),
            }
        ));
        assert!(is_native_reasoning_compatibility_error(
            &ProviderError::Http {
                status: 400,
                body: "bad request: extra inputs are not permitted for reasoning".into(),
            }
        ));
        assert!(!is_native_reasoning_compatibility_error(
            &ProviderError::Auth("bad key".into(),)
        ));
        assert!(!is_native_reasoning_compatibility_error(
            &ProviderError::ContextOverflow("too many tokens".into())
        ));
        assert!(!is_native_reasoning_compatibility_error(
            &ProviderError::RateLimited {
                retry_after_ms: Some(500)
            }
        ));
        assert!(!is_native_reasoning_compatibility_error(
            &ProviderError::Http {
                status: 400,
                body: "missing required field messages".into(),
            }
        ));
    }

    #[test]
    fn backend_defaults_resolve_conservatively_for_local_models() {
        let provider = OpenAIProvider::new();
        let options = StreamOptions {
            thinking: ThinkingLevel::High,
            max_tokens: Some(8_192),
            ..StreamOptions::default()
        };
        let ollama =
            sample_heuristic_local_model("ollama", "http://localhost:11434/v1", "qwen3:32b");
        let lmstudio =
            sample_heuristic_local_model("lmstudio", "http://localhost:1234/v1", "qwen3:32b");
        let vllm = sample_heuristic_local_model("vllm", "http://localhost:8000/v1", "qwen3:32b");
        let unknown =
            sample_heuristic_local_model("custom", "http://localhost:8080/v1", "unknown-model");

        assert_eq!(
            provider.native_reasoning_request_strategies(&ollama, &options),
            vec![
                NativeReasoningRequestStrategy::TopLevelReasoningEffort,
                NativeReasoningRequestStrategy::NoNativeFields,
            ]
        );
        assert_eq!(
            provider.native_reasoning_request_strategies(&lmstudio, &options),
            vec![
                NativeReasoningRequestStrategy::LmStudioNestedReasoning,
                NativeReasoningRequestStrategy::NoNativeFields,
            ]
        );
        assert_eq!(
            provider.native_reasoning_request_strategies(&vllm, &options),
            vec![
                NativeReasoningRequestStrategy::TopLevelReasoningEffort,
                NativeReasoningRequestStrategy::NoNativeFields,
            ]
        );
        assert_eq!(
            effective_reasoning_capabilities(&unknown),
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Prompt),
                output: None,
                tags: None,
                request_mode: Some(ThinkingRequestMode::PromptSteering),
            })
        );
        assert_eq!(
            provider.native_reasoning_request_strategies(&unknown, &options),
            vec![NativeReasoningRequestStrategy::NoNativeFields]
        );
    }

    #[test]
    fn local_reasoning_token_headroom_changes_predictably_with_thinking_level() {
        let model =
            sample_heuristic_local_model("ollama", "http://localhost:11434/v1", "qwen3:32b");

        let off = effective_max_tokens(
            &model,
            &StreamOptions {
                thinking: ThinkingLevel::Off,
                max_tokens: Some(8_192),
                ..StreamOptions::default()
            },
        );
        let low = effective_max_tokens(
            &model,
            &StreamOptions {
                thinking: ThinkingLevel::Low,
                max_tokens: Some(8_192),
                ..StreamOptions::default()
            },
        );
        let medium = effective_max_tokens(
            &model,
            &StreamOptions {
                thinking: ThinkingLevel::Medium,
                max_tokens: Some(8_192),
                ..StreamOptions::default()
            },
        );
        let high = effective_max_tokens(
            &model,
            &StreamOptions {
                thinking: ThinkingLevel::High,
                max_tokens: Some(8_192),
                ..StreamOptions::default()
            },
        );
        let hosted = effective_max_tokens(
            &sample_model(),
            &StreamOptions {
                thinking: ThinkingLevel::High,
                max_tokens: Some(8_192),
                ..StreamOptions::default()
            },
        );

        assert_eq!(off, Some(8_192));
        assert_eq!(low, Some(7_245));
        assert_eq!(medium, Some(6_298));
        assert_eq!(high, Some(5_632));
        assert_eq!(hosted, Some(8_192));
    }
}
