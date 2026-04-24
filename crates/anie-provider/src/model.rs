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
    /// Which outbound field name carries the output-token cap.
    /// `None` (the default) emits `max_tokens`, matching anie's
    /// existing behavior and the most widely-accepted wire
    /// form. Set to `MaxCompletionTokens` for catalog entries
    /// whose upstream rejects the legacy name — OpenAI's
    /// o-series and GPT-5 family post-2024, for example. pi's
    /// default is `max_completion_tokens`; we deviate for
    /// backward compat with the broader set of OpenAI-compat
    /// servers anie talks to (local, proxied, older).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
}

/// Outbound wire-name for the output-token cap.
///
/// OpenAI renamed `max_tokens` → `max_completion_tokens` when
/// they shipped the o-series reasoning models, deprecating the
/// legacy name for those models. Older OpenAI-compat servers
/// still only understand `max_tokens`. Per-model selection via
/// `OpenAICompletionsCompat::max_tokens_field`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// Legacy name, still accepted by most OpenAI-compat servers.
    MaxTokens,
    /// New name required by OpenAI's o-series + GPT-5 endpoints.
    MaxCompletionTokens,
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
    /// Provider-reported capability tokens (e.g. `"vision"`,
    /// `"tools"`, `"thinking"`). Populated by Ollama's
    /// `/api/show.capabilities` array. `None` for endpoints that
    /// don't expose this. Distinct from `supported_parameters`,
    /// which is OpenRouter's request-side parameter list. See
    /// `docs/ollama_capability_discovery/README.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_capabilities: Option<Vec<String>>,
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
    ///
    /// `max_tokens` is recorded on the `Model` for operator
    /// reference and for the compaction summarization path (which
    /// explicitly caps its output). The *main agent stream* does
    /// not forward this value — see
    /// `docs/max_tokens_handling/README.md` and the comment at
    /// `agent_loop.rs` where `StreamOptions::max_tokens = None`.
    /// That means this number doesn't need to be a carefully-
    /// clamped "safe to send" value anymore; the provider-advertised
    /// `max_output_tokens` is carried through verbatim when present,
    /// and a simple fallback is used otherwise.
    #[must_use]
    pub fn to_model(&self, api: ApiKind, base_url: &str) -> Model {
        // Regression guard for Ollama: `/api/show` exposes the
        // model's architectural max context length (e.g. 262 144
        // for qwen3.5), but Ollama's OpenAI-compat endpoint
        // defaults `num_ctx` to 4 096 on the wire and silently
        // ignores attempts to set it. Propagating the discovered
        // value would make compaction grow conversations to ~250 k
        // tokens before trimming, and Ollama would silently
        // truncate the prompt. Keep the conservative 32 k fallback
        // until the deferred native `/api/chat` codepath can honor
        // `num_ctx`. The raw discovered length rides along in
        // `ModelInfo.context_length` so the native plan picks it
        // up without re-discovering.
        let context_window = if self.provider.eq_ignore_ascii_case("ollama") {
            32_768
        } else {
            self.context_length.unwrap_or(32_768)
        };
        let max_tokens = self.max_output_tokens.unwrap_or(8_192);
        Model {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            api,
            base_url: base_url.to_string(),
            context_window,
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
            provider_capabilities: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_model_preserves_advertised_max_output_tokens_verbatim() {
        // After max_tokens/PR1, the main agent stream doesn't
        // forward this value onto the wire (the compaction path
        // still does, but caps explicitly). So we carry whatever
        // the provider advertised — no client-side clamping, no
        // bumping. Matches pi's approach of letting the upstream
        // own the `input + output <= context_window` invariant.
        let info = ModelInfo {
            id: "x/huge-context-model".into(),
            name: "huge".into(),
            provider: "openrouter".into(),
            context_length: Some(262_144),
            max_output_tokens: Some(262_144),
            supports_images: Some(false),
            supports_reasoning: Some(true),
            pricing: None,
            supported_parameters: None,
            provider_capabilities: None,
        };
        let model = info.to_model(ApiKind::OpenAICompletions, "https://openrouter.ai/api/v1");
        assert_eq!(model.max_tokens, 262_144);
        assert_eq!(model.context_window, 262_144);
    }

    #[test]
    fn to_model_does_not_propagate_ollama_context_length_until_native_path() {
        // Regression guard — see the to_model comment and
        // docs/ollama_capability_discovery/README.md. Discovery
        // succeeds (context_length = 262 144 for qwen3.5 from
        // /api/show), but the Model keeps the 32 k fallback
        // until the native /api/chat codepath can honor
        // num_ctx on the wire. If this test starts failing, the
        // deferred native plan has likely shipped — flip the
        // assertion to match the discovered value.
        let info = ModelInfo {
            id: "qwen3.5:9b".into(),
            name: "Qwen 3.5 9B".into(),
            provider: "ollama".into(),
            context_length: Some(262_144),
            max_output_tokens: None,
            supports_images: Some(false),
            supports_reasoning: Some(false),
            pricing: None,
            supported_parameters: None,
            provider_capabilities: Some(vec!["completion".into(), "tools".into()]),
        };
        let model = info.to_model(ApiKind::OpenAICompletions, "http://localhost:11434/v1");
        assert_eq!(model.context_window, 32_768);
    }

    #[test]
    fn to_model_propagates_non_ollama_context_length_unchanged() {
        // Non-Ollama paths must continue to honor the advertised
        // context length — the wire-layer-default regression only
        // applies to Ollama's OpenAI-compat endpoint.
        let info = ModelInfo {
            id: "x/huge".into(),
            name: "Huge".into(),
            provider: "openrouter".into(),
            context_length: Some(200_000),
            max_output_tokens: None,
            supports_images: Some(false),
            supports_reasoning: Some(true),
            pricing: None,
            supported_parameters: None,
            provider_capabilities: None,
        };
        let model = info.to_model(ApiKind::OpenAICompletions, "https://openrouter.ai/api/v1");
        assert_eq!(model.context_window, 200_000);
    }

    #[test]
    fn to_model_falls_back_to_eight_k_when_no_max_is_advertised() {
        // Catalog-level fallback for operators or for the
        // compaction path. Simpler than the reasoning-specific
        // bump we used to do — we don't need a higher default
        // anymore because the main stream ignores this value.
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
            provider_capabilities: None,
        };
        let model = info.to_model(ApiKind::OpenAICompletions, "https://api.openai.com/v1");
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
            provider_capabilities: None,
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
            ..Default::default()
        });
        let model = sample_model(compat.clone());
        let json = serde_json::to_string(&model).expect("serialize model");
        assert!(json.contains("\"kind\":\"openai-completions\""));
        assert!(json.contains("\"openrouter_routing\""));

        let roundtrip: Model = serde_json::from_str(&json).expect("deserialize model");
        assert_eq!(roundtrip.compat, compat);
    }

    #[test]
    fn max_tokens_field_defaults_to_none_and_is_skipped_on_serialize() {
        let compat = OpenAICompletionsCompat::default();
        let json = serde_json::to_string(&compat).expect("serialize");
        // Both optional fields should be skipped → the struct
        // serializes as an empty object.
        assert_eq!(json, "{}");
        let roundtrip: OpenAICompletionsCompat = serde_json::from_str(&json).expect("deserialize");
        assert!(roundtrip.max_tokens_field.is_none());
    }

    #[test]
    fn max_tokens_field_variants_roundtrip() {
        for variant in [
            MaxTokensField::MaxTokens,
            MaxTokensField::MaxCompletionTokens,
        ] {
            let compat = OpenAICompletionsCompat {
                max_tokens_field: Some(variant),
                ..Default::default()
            };
            let json = serde_json::to_string(&compat).expect("serialize");
            let roundtrip: OpenAICompletionsCompat =
                serde_json::from_str(&json).expect("deserialize");
            assert_eq!(roundtrip.max_tokens_field, Some(variant));
        }
    }

    #[test]
    fn max_tokens_field_serializes_as_snake_case() {
        // Wire-compat: if we ever surface these through config
        // they should be readable. snake_case matches the actual
        // OpenAI field names.
        let json = serde_json::to_string(&MaxTokensField::MaxCompletionTokens).unwrap();
        assert_eq!(json, "\"max_completion_tokens\"");
        let json = serde_json::to_string(&MaxTokensField::MaxTokens).unwrap();
        assert_eq!(json, "\"max_tokens\"");
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
