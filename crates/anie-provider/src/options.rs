use std::collections::HashMap;

use anie_protocol::ToolDef;

use crate::ThinkingLevel;

/// Provider-native message representation.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmMessage {
    /// Native role string.
    pub role: String,
    /// Native content payload.
    pub content: serde_json::Value,
}

/// Full request context for a streaming LLM call.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmContext {
    /// Final system prompt string.
    pub system_prompt: String,
    /// Provider-native messages.
    pub messages: Vec<LlmMessage>,
    /// Registered tools available to the model.
    pub tools: Vec<ToolDef>,
}

/// Options passed directly to a provider stream request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamOptions {
    /// Optional API key.
    pub api_key: Option<String>,
    /// Optional temperature override.
    pub temperature: Option<f32>,
    /// Optional max output tokens override.
    pub max_tokens: Option<u64>,
    /// Requested reasoning level.
    pub thinking: ThinkingLevel,
    /// Extra headers applied to the request.
    pub headers: HashMap<String, String>,
    /// Override for Ollama's `options.num_ctx`. `None` means the
    /// provider falls back to `Model.context_window`.
    pub num_ctx_override: Option<u64>,
}

/// Request-specific auth and routing resolved just before a provider call.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRequestOptions {
    /// Optional API key.
    pub api_key: Option<String>,
    /// Extra per-request headers.
    pub headers: HashMap<String, String>,
    /// Optional base-URL override.
    pub base_url_override: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_options_num_ctx_override_default_is_none() {
        assert_eq!(StreamOptions::default().num_ctx_override, None);
    }
}
