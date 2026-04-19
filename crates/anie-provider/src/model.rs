use serde::{Deserialize, Serialize};

use crate::ApiKind;

/// Cost metadata reported per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CostPerMillion {
    /// Input token cost.
    pub input: f64,
    /// Output token cost.
    pub output: f64,
    /// Cache read token cost.
    pub cache_read: f64,
    /// Cache write token cost.
    pub cache_write: f64,
}

impl CostPerMillion {
    /// A zero-cost pricing record, useful for local models.
    pub fn zero() -> Self {
        Self::default()
    }
}

/// How a model accepts reasoning control on requests.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReasoningControlMode {
    /// Provider-owned prompt steering or similar soft guidance.
    Prompt,
    /// Native backend request fields.
    Native,
}

/// How a model exposes reasoning in responses.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReasoningOutputMode {
    /// Reasoning is embedded inline with ordinary text using tags.
    Tagged,
    /// Reasoning is streamed separately from visible answer text.
    Separated,
}

/// How thinking should be requested for a model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThinkingRequestMode {
    /// Use prompt-steering text to encourage or discourage reasoning.
    PromptSteering,
    /// Use the top-level `reasoning_effort` field.
    ReasoningEffort,
    /// Use a nested `reasoning.effort` field.
    NestedReasoning,
}

/// Explicit opening/closing tags used for tagged reasoning output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningTags {
    /// Opening reasoning tag, e.g. `<think>`.
    pub open: String,
    /// Closing reasoning tag, e.g. `</think>`.
    pub close: String,
}

/// Richer reasoning metadata attached to a model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningCapabilities {
    /// Request-side reasoning control behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ReasoningControlMode>,
    /// Response-side reasoning output behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ReasoningOutputMode>,
    /// Optional explicit tags for tagged reasoning output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<ReasoningTags>,
    /// Optional explicit request-shape hint for thinking support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_mode: Option<ThinkingRequestMode>,
}

/// Round-trip / replay requirements that vary per model (not per
/// provider). Populated in the model catalog for known models; `None`
/// on `Model` means "no special replay requirements" (the default
/// for OpenAI chat-completions, local models, etc.).
///
/// See docs/api_integrity_plans/03c_replay_capabilities.md.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayCapabilities {
    /// The provider requires every replayed thinking block to carry
    /// the cryptographic `signature` the API issued originally.
    /// Set on Anthropic Claude models with extended thinking support.
    #[serde(default)]
    pub requires_thinking_signature: bool,

    /// The provider can emit `redacted_thinking` blocks (opaque
    /// encrypted reasoning) that must be replayed verbatim. Used by
    /// plan 02.
    #[serde(default)]
    pub supports_redacted_thinking: bool,

    /// The provider's response contains an opaque
    /// `encrypted_content` that must be replayed to continue the
    /// reasoning chain. Reserved for future OpenAI Responses API
    /// support; currently false everywhere.
    #[serde(default)]
    pub supports_encrypted_reasoning: bool,
}

/// A model discovered from a provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelInfo {
    /// Model identifier as reported by the endpoint.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Provider identifier.
    pub provider: String,
    /// Provider-advertised context window, when known.
    pub context_length: Option<u64>,
    /// Whether the model accepts images, when known.
    pub supports_images: Option<bool>,
    /// Whether the model supports reasoning features, when known.
    pub supports_reasoning: Option<bool>,
}

/// Registered model metadata used to route and parameterize provider calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Model {
    /// Model identifier sent to the provider API.
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Provider identifier.
    pub provider: String,
    /// Provider wire protocol.
    pub api: ApiKind,
    /// Default provider base URL.
    pub base_url: String,
    /// Provider-advertised context window.
    pub context_window: u64,
    /// Maximum output tokens to request.
    pub max_tokens: u64,
    /// Whether the model supports reasoning features.
    pub supports_reasoning: bool,
    /// Optional richer reasoning metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_capabilities: Option<ReasoningCapabilities>,
    /// Whether the model accepts images.
    pub supports_images: bool,
    /// Pricing metadata.
    pub cost_per_million: CostPerMillion,
    /// Round-trip / replay requirements. `None` = no special
    /// requirements. See `ReplayCapabilities`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_capabilities: Option<ReplayCapabilities>,
}

impl Model {
    /// Return the effective replay capabilities for this model,
    /// falling back to `ReplayCapabilities::default()` (all false)
    /// when nothing is declared.
    #[must_use]
    pub fn effective_replay_capabilities(&self) -> ReplayCapabilities {
        self.replay_capabilities.clone().unwrap_or_default()
    }
}

impl ModelInfo {
    /// Convert a discovered model into a runtime model definition using conservative defaults.
    #[must_use]
    pub fn to_model(&self, api: ApiKind, base_url: &str) -> Model {
        Model {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            api,
            base_url: base_url.to_string(),
            context_window: self.context_length.unwrap_or(32_768),
            max_tokens: 8_192,
            supports_reasoning: self.supports_reasoning.unwrap_or(false),
            reasoning_capabilities: None,
            supports_images: self.supports_images.unwrap_or(false),
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
        }
    }
}

impl From<&Model> for ModelInfo {
    fn from(value: &Model) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            provider: value.provider.clone(),
            context_length: Some(value.context_window),
            supports_images: Some(value.supports_images),
            supports_reasoning: Some(value.supports_reasoning),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_info_to_model_uses_conservative_defaults() {
        let info = ModelInfo {
            id: "qwen3:32b".into(),
            name: "Qwen 3 32B".into(),
            provider: "ollama".into(),
            context_length: None,
            supports_images: None,
            supports_reasoning: None,
        };

        let model = info.to_model(ApiKind::OpenAICompletions, "http://localhost:11434/v1");
        assert_eq!(model.id, "qwen3:32b");
        assert_eq!(model.name, "Qwen 3 32B");
        assert_eq!(model.provider, "ollama");
        assert_eq!(model.api, ApiKind::OpenAICompletions);
        assert_eq!(model.base_url, "http://localhost:11434/v1");
        assert_eq!(model.context_window, 32_768);
        assert_eq!(model.max_tokens, 8_192);
        assert!(!model.supports_reasoning);
        assert!(!model.supports_images);
        assert_eq!(model.cost_per_million, CostPerMillion::zero());
        assert_eq!(model.reasoning_capabilities, None);
    }
}
