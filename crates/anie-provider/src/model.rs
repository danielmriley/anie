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
}
