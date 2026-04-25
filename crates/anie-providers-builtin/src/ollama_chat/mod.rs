//! Ollama native `/api/chat` provider scaffold.
//!
//! anie-specific (not in pi): pi uses Ollama's OpenAI-compatible
//! endpoint, but that path cannot honor `think: false` or
//! `options.num_ctx`. The real NDJSON implementation lands in the
//! follow-up PRs; this scaffold exists so catalog entries fail with a
//! typed, actionable error instead of a missing-provider error.

use anie_protocol::{ContentBlock, Message, ToolDef};
use anie_provider::{
    LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderStream, StreamOptions,
};

/// Ollama native `/api/chat` provider implementation.
#[derive(Clone)]
pub struct OllamaChatProvider {
    client: reqwest::Client,
}

impl OllamaChatProvider {
    /// Create a new provider using the workspace-shared HTTP client.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: crate::http::shared_http_client()
                .cloned()
                .unwrap_or_else(|_| crate::http::create_http_client()),
        }
    }
}

impl Default for OllamaChatProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for OllamaChatProvider {
    fn stream(
        &self,
        _model: &Model,
        _context: LlmContext,
        _options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let _client = &self.client;
        Err(ProviderError::FeatureUnsupported(
            "OllamaChatApi native /api/chat streaming is scaffolded but not implemented yet; use an Ollama model configured with OpenAICompletions until the native streaming PR lands"
                .to_string(),
        ))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
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

    fn convert_tools(&self, _tools: &[ToolDef]) -> Vec<serde_json::Value> {
        Vec::new()
    }
}

fn text_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use anie_provider::{ApiKind, CostPerMillion, ModelCompat, ProviderRegistry, ThinkingLevel};

    use super::*;

    fn ollama_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OllamaChatApi,
            base_url: "http://localhost:11434".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    fn empty_context() -> LlmContext {
        LlmContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn scaffold_returns_feature_unsupported_error_with_actionable_message() {
        let provider = OllamaChatProvider::new();
        let Err(error) = provider.stream(
            &ollama_model(),
            empty_context(),
            StreamOptions {
                thinking: ThinkingLevel::Off,
                ..StreamOptions::default()
            },
        ) else {
            panic!("placeholder should reject streaming");
        };

        let ProviderError::FeatureUnsupported(message) = error else {
            panic!("expected feature unsupported error");
        };
        assert!(message.contains("OllamaChatApi"));
        assert!(message.contains("OpenAICompletions"));
    }

    #[test]
    fn registry_routes_ollama_chat_api_to_placeholder_until_pr3() {
        let mut registry = ProviderRegistry::new();
        crate::register_builtin_providers(&mut registry);

        let Err(error) =
            registry.stream(&ollama_model(), empty_context(), StreamOptions::default())
        else {
            panic!("registered placeholder should reject streaming");
        };

        assert!(matches!(error, ProviderError::FeatureUnsupported(_)));
    }
}
