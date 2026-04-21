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

    /// The provider emits `reasoning_details` (OpenRouter's
    /// normalized wrapper over encrypted reasoning blobs from
    /// o-series / GPT-5 upstreams) that must be replayed verbatim
    /// on subsequent turns or the upstream drops reasoning
    /// context. Currently set for OpenRouter catalog entries
    /// whose id matches `openai/o*` or `openai/gpt-5*`.
    #[serde(default)]
    pub supports_reasoning_details_replay: bool,
}

/// Provider-family compat knobs attached per model.
///
/// Each variant collects the flags that are semantically
/// meaningful for one `ApiKind` family. Variants are open — the
/// fields inside a variant are all optional so adding one later
/// doesn't break serde roundtrips, and deserializing an older
/// session file without the new field is safe.
///
/// This is the anie equivalent of pi's per-model compat blobs
/// (pi: `packages/ai/src/types.ts:265` `OpenAICompletionsCompat`).
/// It lets one provider module (the `OpenAICompletions` provider)
/// cover many vendors — OpenAI, OpenRouter, xAI, Groq, Cerebras,
/// local llama.cpp, etc. — by flipping flags per catalog entry
/// instead of growing the `Provider` trait.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ModelCompat {
    /// No compat knobs applicable for this model.
    #[default]
    None,
    /// Compat knobs for models served by the OpenAI
    /// Chat-Completions-compatible wire protocol.
    #[serde(rename = "openai-completions")]
    OpenAICompletions(OpenAICompletionsCompat),
}

/// Compat knobs for models on the `OpenAICompletions` wire
/// protocol. Currently only carries OpenRouter routing
/// preferences; future plans add fields for xAI, Groq, Azure,
/// etc., without breaking existing catalog entries.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpenAICompletionsCompat {
    /// OpenRouter provider-routing preferences. Only meaningful
    /// when `base_url` resolves to OpenRouter; ignored
    /// otherwise. When `None`, no `provider` field is emitted
    /// into the outbound request body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openrouter_routing: Option<OpenRouterRouting>,
}

/// OpenRouter provider-routing preferences.
///
/// Subset of the shape OpenRouter accepts at
/// <https://openrouter.ai/docs/provider-routing>. Only the
/// fields we use in v1 are present; additional fields can be
/// added later without breaking existing serialized values
/// (every field is `Option<T>` with `skip_serializing_if`).
///
/// Consumed by the outbound-request builder when the target
/// base URL resolves to OpenRouter and the model's compat blob
/// carries this struct. The whole value serializes as the
/// top-level `provider` object in the Chat Completions request
/// body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OpenRouterRouting {
    /// Whether OpenRouter may fall back to upstreams outside
    /// `order`/`only` if none of them are healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    /// Ordered upstream provider slugs OpenRouter should try in
    /// sequence, falling back to the next on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Vec<String>>,
    /// Exclusive upstream allowlist for this request. Any
    /// upstream not listed is skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
    /// Upstream provider slugs to skip for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    /// Restrict routing to upstreams that offer Zero-Data-
    /// Retention endpoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zdr: Option<bool>,
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
    /// Provider-advertised cap on *output* tokens, when known.
    /// OpenRouter reports this in `top_provider.max_completion_tokens`;
    /// honoring it prevents `max_tokens` from defaulting to a
    /// value too small for reasoning models that produce several
    /// thousand tokens of reasoning before answering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Whether the model accepts images, when known.
    pub supports_images: Option<bool>,
    /// Whether the model supports reasoning features, when known.
    pub supports_reasoning: Option<bool>,
    /// Per-token pricing reported by the provider (currently
    /// populated only by OpenRouter; `None` elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// Feature flags reported by the provider (e.g. `"tools"`,
    /// `"reasoning"`, `"tool_choice"`). Populated by OpenRouter's
    /// `/models` response; `None` for endpoints that don't report
    /// this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_parameters: Option<Vec<String>>,
}

/// Per-token pricing reported by a discovery endpoint. OpenRouter
/// returns prices as string decimals (per token, not per million),
/// so fields are kept as `String` to preserve precision and match
/// the upstream format without lossy conversion.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ModelPricing {
    /// Input-token price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Output-token price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion: Option<String>,
    /// Flat per-request surcharge, when the provider charges one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<String>,
    /// Per-image surcharge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
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
    /// Per-model provider-family compat knobs. `None` = nothing
    /// special. See `ModelCompat`.
    #[serde(default, skip_serializing_if = "is_default_compat")]
    pub compat: ModelCompat,
}

/// Helper for `Model::compat`'s `skip_serializing_if` so that
/// `ModelCompat::None` is written as an omitted field rather than
/// `{"kind":"none"}`. Keeps existing catalogs and session files
/// byte-identical on the wire until a non-default compat is
/// populated.
fn is_default_compat(compat: &ModelCompat) -> bool {
    matches!(compat, ModelCompat::None)
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
        // Prefer the provider-advertised output-token cap when we
        // have one (OpenRouter reports it via
        // `top_provider.max_completion_tokens`). For reasoning-
        // capable models without an advertised cap, bump the
        // default to 32 k — the 8 k default routinely clips
        // reasoning upstreams mid-thought and surfaces as a
        // `ResponseTruncated` error. For non-reasoning models
        // lacking an advertised cap, 8 k stays fine.
        let max_tokens = self.max_output_tokens.unwrap_or_else(|| {
            if self.supports_reasoning.unwrap_or(false) {
                32_768
            } else {
                8_192
            }
        });
        Model {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            api,
            base_url: base_url.to_string(),
            context_window: self.context_length.unwrap_or(32_768),
            max_tokens,
            supports_reasoning: self.supports_reasoning.unwrap_or(false),
            reasoning_capabilities: None,
            supports_images: self.supports_images.unwrap_or(false),
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
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
            max_output_tokens: Some(value.max_tokens),
            supports_images: Some(value.supports_images),
            supports_reasoning: Some(value.supports_reasoning),
            pricing: None,
            supported_parameters: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_model_honors_advertised_max_output_tokens() {
        let info = ModelInfo {
            id: "openai/o3".into(),
            name: "o3".into(),
            provider: "openrouter".into(),
            context_length: Some(128_000),
            max_output_tokens: Some(65_536),
            supports_images: Some(false),
            supports_reasoning: Some(true),
            pricing: None,
            supported_parameters: None,
        };
        let model = info.to_model(
            ApiKind::OpenAICompletions,
            "https://openrouter.ai/api/v1",
        );
        assert_eq!(model.max_tokens, 65_536);
    }

    #[test]
    fn to_model_bumps_default_for_reasoning_models_without_advertised_cap() {
        // Regression: 8 k default was too small for reasoning
        // upstreams that emit several thousand tokens of
        // reasoning before answering — surfaced as
        // `ResponseTruncated` in the wild. Reasoning-capable
        // models without an advertised cap now get 32 k.
        let info = ModelInfo {
            id: "nvidia/nemotron-3-super-120b-a12b:free".into(),
            name: "Nemotron".into(),
            provider: "openrouter".into(),
            context_length: Some(131_072),
            max_output_tokens: None,
            supports_images: Some(false),
            supports_reasoning: Some(true),
            pricing: None,
            supported_parameters: None,
        };
        let model = info.to_model(
            ApiKind::OpenAICompletions,
            "https://openrouter.ai/api/v1",
        );
        assert_eq!(model.max_tokens, 32_768);
    }

    #[test]
    fn to_model_keeps_conservative_default_for_non_reasoning_models() {
        let info = ModelInfo {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            context_length: Some(128_000),
            max_output_tokens: None,
            supports_images: Some(true),
            supports_reasoning: Some(false),
            pricing: None,
            supported_parameters: None,
        };
        let model = info.to_model(
            ApiKind::OpenAICompletions,
            "https://api.openai.com/v1",
        );
        assert_eq!(model.max_tokens, 8_192);
    }

    #[test]
    fn model_info_to_model_uses_conservative_defaults() {
        let info = ModelInfo {
            id: "qwen3:32b".into(),
            name: "Qwen 3 32B".into(),
            provider: "ollama".into(),
            context_length: None,
            max_output_tokens: None,
            supports_images: None,
            supports_reasoning: None,
            pricing: None,
            supported_parameters: None,
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
        assert_eq!(model.compat, ModelCompat::None);
    }

    fn sample_model(compat: ModelCompat) -> Model {
        Model {
            id: "m".into(),
            name: "M".into(),
            provider: "openrouter".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://openrouter.ai/api/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat,
        }
    }

    #[test]
    fn model_compat_defaults_to_none_and_is_skipped_on_serialize() {
        let model = sample_model(ModelCompat::None);
        let json = serde_json::to_string(&model).expect("serialize model");
        assert!(
            !json.contains("\"compat\""),
            "default compat must not appear in serialized form: {json}"
        );

        let roundtrip: Model = serde_json::from_str(&json).expect("deserialize model");
        assert_eq!(roundtrip.compat, ModelCompat::None);
    }

    #[test]
    fn model_compat_with_openai_completions_roundtrips() {
        let compat = ModelCompat::OpenAICompletions(OpenAICompletionsCompat {
            openrouter_routing: Some(OpenRouterRouting {
                allow_fallbacks: Some(true),
                order: Some(vec!["anthropic".into(), "openai".into()]),
                only: None,
                ignore: None,
                zdr: Some(true),
            }),
        });
        let model = sample_model(compat.clone());
        let json = serde_json::to_string(&model).expect("serialize model");
        assert!(json.contains("\"kind\":\"openai-completions\""));
        assert!(json.contains("\"openrouter_routing\""));

        let roundtrip: Model = serde_json::from_str(&json).expect("deserialize model");
        assert_eq!(roundtrip.compat, compat);
    }

    #[test]
    fn openrouter_routing_default_has_no_preferences() {
        let routing = OpenRouterRouting::default();
        assert_eq!(routing.allow_fallbacks, None);
        assert_eq!(routing.order, None);
        assert_eq!(routing.only, None);
        assert_eq!(routing.ignore, None);
        assert_eq!(routing.zdr, None);

        let json = serde_json::to_string(&routing).expect("serialize routing");
        assert_eq!(json, "{}");
    }

    #[test]
    fn openrouter_routing_roundtrips_with_only_populated_fields() {
        let routing = OpenRouterRouting {
            allow_fallbacks: None,
            order: Some(vec!["groq".into()]),
            only: Some(vec!["groq".into(), "fireworks".into()]),
            ignore: None,
            zdr: None,
        };

        let json = serde_json::to_string(&routing).expect("serialize routing");
        assert!(!json.contains("allow_fallbacks"));
        assert!(!json.contains("ignore"));
        assert!(!json.contains("zdr"));
        assert!(json.contains("\"order\""));
        assert!(json.contains("\"only\""));

        let roundtrip: OpenRouterRouting =
            serde_json::from_str(&json).expect("deserialize routing");
        assert_eq!(roundtrip, routing);
    }
}
