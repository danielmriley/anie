//! `MessageSummarizer` implementation that calls the real LLM stack.
//!
//! The session crate only knows how to sequence compaction against
//! a `MessageSummarizer` trait — the actual LLM request happens here,
//! so the session crate stays provider-agnostic.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use futures::{StreamExt, pin_mut};

use anie_protocol::{AssistantMessage, ContentBlock, Message, UserMessage, now_millis};
use anie_provider::{
    LlmContext, Model, ProviderEvent, ProviderRegistry, RequestOptionsResolver, StreamOptions,
    ThinkingLevel,
};
use anie_session::{MessageSummarizer, build_compaction_prompt};

/// Compaction summarizer that runs against the live provider stack.
///
/// Owns shared references to the provider registry and request-options
/// resolver, plus a snapshot of the model to summarize with. Built
/// per-compaction-call from `ControllerState`'s current state so it
/// reflects whatever model / config is active at the time of the
/// compaction.
pub(crate) struct CompactionStrategy {
    model: Model,
    registry: Arc<ProviderRegistry>,
    resolver: Arc<dyn RequestOptionsResolver>,
    num_ctx_override: Option<u64>,
}

impl CompactionStrategy {
    pub(crate) fn new(
        model: Model,
        registry: Arc<ProviderRegistry>,
        resolver: Arc<dyn RequestOptionsResolver>,
        num_ctx_override: Option<u64>,
    ) -> Self {
        Self {
            model,
            registry,
            resolver,
            num_ctx_override,
        }
    }
}

#[async_trait]
impl MessageSummarizer for CompactionStrategy {
    async fn summarize(
        &self,
        messages: &[Message],
        existing_summary: Option<&str>,
    ) -> Result<String> {
        let prompt = build_compaction_prompt(messages, existing_summary);

        let summary_prompt = vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: prompt }],
            timestamp: now_millis(),
        })];

        let request = self
            .resolver
            .resolve(&self.model, messages)
            .await
            .map_err(anyhow::Error::from)?;
        let provider = self
            .registry
            .get(&self.model.api)
            .ok_or_else(|| anyhow!("no provider registered for {:?}", self.model.api))?;

        let mut resolved_model = self.model.clone();
        if let Some(base_url_override) = request.base_url_override {
            resolved_model.base_url = base_url_override;
        }

        let llm_context = LlmContext {
            system_prompt: "You summarize coding-assistant sessions so work can continue after context compaction. Preserve goals, progress, key decisions, file paths, and remaining tasks.".into(),
            messages: provider.convert_messages(&summary_prompt),
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: request.api_key,
            temperature: None,
            max_tokens: Some(resolved_model.max_tokens.min(4_096)),
            thinking: ThinkingLevel::Off,
            headers: request.headers,
            num_ctx_override: self.num_ctx_override,
        };

        let stream = provider
            .stream(&resolved_model, llm_context, options)
            .map_err(anyhow::Error::from)?;
        pin_mut!(stream);

        let mut collected = String::new();
        while let Some(event) = stream.next().await {
            match event.map_err(anyhow::Error::from)? {
                ProviderEvent::TextDelta(text) | ProviderEvent::ThinkingDelta(text) => {
                    collected.push_str(&text);
                }
                ProviderEvent::Done(message) => {
                    if collected.trim().is_empty() {
                        collected = join_assistant_text(&message);
                    }
                    break;
                }
                ProviderEvent::Start
                | ProviderEvent::ToolCallStart(_)
                | ProviderEvent::ToolCallDelta { .. }
                | ProviderEvent::ToolCallEnd { .. } => {}
            }
        }

        let summary = collected.trim().to_string();
        if summary.is_empty() {
            return Err(anyhow!("compaction summary was empty"));
        }
        Ok(summary)
    }
}

fn join_assistant_text(message: &AssistantMessage) -> String {
    // Plan 08 PR-A: the previous shape cloned every visible
    // text / thinking fragment into a `Vec<String>` and then
    // `.join`'d. Replaced with a single-allocation sized-up
    // direct-buffer build that borrows the fragments instead
    // of cloning them.
    let mut total = 0usize;
    let mut first = true;
    for block in &message.content {
        let fragment = match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::Thinking { thinking, .. } => thinking.as_str(),
            _ => continue,
        };
        if !first {
            total += 1;
        }
        total += fragment.len();
        first = false;
    }
    let mut text = String::with_capacity(total);
    let mut first = true;
    for block in &message.content {
        let fragment = match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::Thinking { thinking, .. } => thinking.as_str(),
            _ => continue,
        };
        if !first {
            text.push('\n');
        }
        text.push_str(fragment);
        first = false;
    }
    if text.is_empty() {
        message
            .error_message
            .clone()
            .unwrap_or_else(|| String::from("[empty summary response]"))
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use anie_protocol::{StopReason, ToolDef, Usage};
    use anie_provider::{
        ApiKind, CostPerMillion, LlmMessage, Provider, ProviderStream, ResolvedRequestOptions,
    };
    use async_trait::async_trait;
    use futures::stream;

    use super::*;

    struct StaticResolver;

    #[async_trait]
    impl RequestOptionsResolver for StaticResolver {
        async fn resolve(
            &self,
            _model: &Model,
            _context: &[Message],
        ) -> Result<ResolvedRequestOptions, anie_provider::ProviderError> {
            Ok(ResolvedRequestOptions::default())
        }
    }

    struct RecordingProvider {
        options: Arc<Mutex<Vec<StreamOptions>>>,
    }

    impl Provider for RecordingProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: LlmContext,
            options: StreamOptions,
        ) -> Result<ProviderStream, anie_provider::ProviderError> {
            self.options.lock().expect("record options").push(options);
            let message = AssistantMessage {
                content: vec![ContentBlock::Text {
                    text: "summary".into(),
                }],
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                provider: "ollama".into(),
                model: "qwen3:32b".into(),
                timestamp: 1,
                reasoning_details: None,
            };
            Ok(Box::pin(stream::iter(vec![Ok(ProviderEvent::Done(
                message,
            ))])))
        }

        fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
            messages
                .iter()
                .map(|message| LlmMessage {
                    role: "user".into(),
                    content: serde_json::to_value(message).expect("message json"),
                })
                .collect()
        }

        fn convert_tools(&self, _tools: &[ToolDef]) -> Vec<serde_json::Value> {
            Vec::new()
        }
    }

    fn ollama_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OllamaChatApi,
            base_url: "http://localhost:11434".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: anie_provider::ModelCompat::None,
        }
    }

    #[tokio::test]
    async fn compaction_summary_request_passes_num_ctx_override() {
        let recorded_options = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ProviderRegistry::new();
        registry.register(
            ApiKind::OllamaChatApi,
            Box::new(RecordingProvider {
                options: Arc::clone(&recorded_options),
            }),
        );
        let strategy = CompactionStrategy::new(
            ollama_model(),
            Arc::new(registry),
            Arc::new(StaticResolver),
            Some(16_384),
        );

        let summary = strategy
            .summarize(
                &[Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "source".into(),
                    }],
                    timestamp: 1,
                })],
                None,
            )
            .await
            .expect("summary");

        assert_eq!(summary, "summary");
        let options = recorded_options.lock().expect("recorded options");
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].num_ctx_override, Some(16_384));
    }
}
