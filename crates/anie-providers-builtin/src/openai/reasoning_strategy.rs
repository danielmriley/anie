//! Reasoning-level mapping and OpenAI-compatible backend detection.
//!
//! This module owns the decisions around how anie sends reasoning /
//! thinking requests to OpenAI-compatible targets (hosted OpenAI,
//! Ollama, LM Studio, vLLM, other local servers) and how it interprets
//! the streaming response.
//!
//! The main inputs are `Model` (which carries a provider name, base URL,
//! optional reasoning capabilities, and API kind) and `StreamOptions`
//! (which carries the user-requested thinking level and token budget).

use anie_provider::{
    ApiKind, Model, ProviderError, ReasoningCapabilities, StreamOptions, ThinkingLevel,
};

use crate::local::default_local_reasoning_capabilities;

/// Map a `ThinkingLevel` to the OpenAI `reasoning_effort` string.
pub(super) fn reasoning_effort(thinking: ThinkingLevel) -> Option<&'static str> {
    match thinking {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal"),
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
    }
}

/// True when `model` targets a local OpenAI-compatible server
/// (localhost / 127.0.0.1 / ::1, or a known local provider name).
pub(super) fn is_local_openai_compatible_target(model: &Model) -> bool {
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

/// System-prompt steering text used for local models that don't
/// support native reasoning fields.
pub(super) fn local_reasoning_prompt_steering(thinking: ThinkingLevel) -> &'static str {
    match thinking {
        ThinkingLevel::Off => {
            "For this response, answer directly and avoid a visible reasoning block unless it is necessary."
        }
        ThinkingLevel::Minimal => {
            "For this response, do the minimum reasoning necessary — favor a direct answer unless the question genuinely requires analysis."
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

/// How reasoning controls should be placed in the request body for
/// OpenAI-compatible targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum NativeReasoningRequestStrategy {
    /// Do not send `reasoning_effort` or `reasoning.*` fields.
    NoNativeFields,
    /// Send top-level `reasoning_effort: "low"|"medium"|"high"`.
    /// Used by hosted OpenAI, Ollama, vLLM, and most OpenAI-compat
    /// servers.
    TopLevelReasoningEffort,
    /// Send nested `reasoning: { effort: "..." }`. Used by LM
    /// Studio locally and by hosted OpenRouter (OpenRouter
    /// normalizes reasoning across upstream providers via this
    /// nested object). Unlike `TopLevelReasoningEffort`, this
    /// path emits `{ effort: "none" }` for `ThinkingLevel::Off`
    /// so the upstream receives an explicit disable signal.
    NestedReasoning,
    /// Send `enable_thinking: bool` as a disable-signaling flag.
    /// `nested = false` emits it at the top level (pi's `zai` /
    /// `qwen` formats); `nested = true` emits it inside
    /// `chat_template_kwargs` (pi's `qwen-chat-template`
    /// format). The boolean is derived from `ThinkingLevel`:
    /// `Off` → `false`, any other level → `true`. Intended for
    /// vLLM / SGLang Qwen3+ deployments and Z.ai GLM models.
    EnableThinkingFlag { nested: bool },
}

/// Identifies which OpenAI-compatible backend shape `model` targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenAiCompatibleBackend {
    Hosted,
    Ollama,
    LmStudio,
    Vllm,
    UnknownLocal,
}

/// Return the effective reasoning capabilities for a model:
/// the model's declared capabilities first, falling back to the
/// local-family heuristic when the model has none declared.
pub(super) fn effective_reasoning_capabilities(model: &Model) -> Option<ReasoningCapabilities> {
    model.reasoning_capabilities.clone().or_else(|| {
        default_local_reasoning_capabilities(&model.provider, &model.base_url, &model.id)
    })
}

/// Compute the `max_tokens` value to send, reserving headroom for
/// reasoning output on local targets so a local reasoning model does
/// not consume the entire output budget on internal thoughts.
pub(super) fn effective_max_tokens(model: &Model, options: &StreamOptions) -> Option<u64> {
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
        // Minimal sits between Off and Low — the reasoning
        // output is expected to be very brief.
        ThinkingLevel::Minimal => max_tokens / 20,
        ThinkingLevel::Low => max_tokens / 10,
        ThinkingLevel::Medium => max_tokens / 5,
        ThinkingLevel::High => max_tokens / 4,
    };
    let visible_reasoning_headroom = if visible_reasoning_output_likely {
        match options.thinking {
            ThinkingLevel::Off => 0,
            ThinkingLevel::Minimal => 64,
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

/// Detect the backend shape from the model's provider name / base URL.
pub(super) fn openai_compatible_backend(model: &Model) -> OpenAiCompatibleBackend {
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

/// True when `error` is the typed `NativeReasoningUnsupported`
/// variant, indicating the caller should retry with a
/// `NoNativeFields` request strategy.
///
/// The body-pattern detection lives in `classify_openai_http_error`
/// — this check is a simple typed match so callers don't stringly
/// probe error contents.
pub(super) fn is_native_reasoning_compatibility_error(error: &ProviderError) -> bool {
    matches!(error, ProviderError::NativeReasoningUnsupported(_))
}

/// Classify an OpenAI non-success HTTP response, upgrading to
/// `NativeReasoningUnsupported` when the 400 body matches the
/// known patterns indicating the target rejected our reasoning
/// fields.
///
/// Falls through to the generic `classify_http_error` for every
/// other case. Body-string detection is confined to this one site.
pub(super) fn classify_openai_http_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
) -> ProviderError {
    if status.as_u16() == 400 && looks_like_native_reasoning_compat_body(body) {
        return ProviderError::NativeReasoningUnsupported(body.to_string());
    }
    crate::classify_http_error(status, body, retry_after_ms)
}

fn looks_like_native_reasoning_compat_body(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    let mentions_reasoning_field = body.contains("reasoning_effort")
        || body.contains("reasoning.effort")
        || body.contains("\"reasoning\"")
        || body.contains("'reasoning'")
        || body.contains("reasoning")
        || body.contains(" field required") && body.contains("reasoning")
        // Ollama's native `/api/chat` errors surface through the
        // OpenAI-compat endpoint using the word `think` / `thinking`
        // rather than `reasoning`, e.g.
        //   `think value "low" is not supported for this model`
        //   `"gemma3:1b" does not support thinking`
        // Recognize both so `send_stream_request` retries with
        // `NoNativeFields`. See docs/ollama_capability_discovery
        // PR 2.
        || body.contains("thinking")
        || body.contains("think value")
        || body.contains("think field");
    let indicates_compatibility_failure = body.contains("unknown")
        || body.contains("unsupported")
        || body.contains("unexpected")
        || body.contains("unrecognized")
        || body.contains("extra inputs")
        || body.contains("not permitted")
        || body.contains("additional properties")
        || body.contains("invalid")
        || body.contains("bad request")
        // Ollama phrasings — "is not supported for this model",
        // "does not support thinking".
        || body.contains("not supported")
        || body.contains("does not support");

    mentions_reasoning_field && indicates_compatibility_failure
}

/// Extract a thinking/reasoning delta from a streamed chat-completion
/// `delta` object. Returns the first non-empty value found in any of
/// `reasoning`, `reasoning_content`, `reasoning_text`, or `thinking`.
///
/// The three `reasoning*` names are all seen in the wild: OpenAI and
/// most OpenAI-compat servers use `reasoning` or `reasoning_content`;
/// OpenRouter forwards `reasoning_text` from some upstreams
/// (DeepSeek's native API among them) without normalizing the field
/// name.
pub(super) fn native_reasoning_delta(delta: &serde_json::Value) -> Option<String> {
    [
        "reasoning",
        "reasoning_content",
        "reasoning_text",
        "thinking",
    ]
    .iter()
    .find_map(|field| {
        delta
            .get(*field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use anie_provider::{CostPerMillion, ModelCompat, ReasoningControlMode, ThinkingRequestMode};

    use super::*;
    use crate::OpenAIProvider;

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
            cost_per_million: CostPerMillion::zero(),
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
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    #[test]
    fn native_reasoning_delta_captures_every_known_field_name() {
        use serde_json::json;
        for field in [
            "reasoning",
            "reasoning_content",
            "reasoning_text",
            "thinking",
        ] {
            let delta = json!({ field: "thinking hard" });
            assert_eq!(
                native_reasoning_delta(&delta),
                Some("thinking hard".to_string()),
                "field {field:?} should be recognized"
            );
        }
    }

    #[test]
    fn native_reasoning_delta_returns_none_when_no_recognized_field_present() {
        let delta = serde_json::json!({ "content": "plain text", "role": "assistant" });
        assert_eq!(native_reasoning_delta(&delta), None);
    }

    #[test]
    fn reasoning_effort_maps_minimal_level_to_minimal_string() {
        // Plan 01 PR B: GPT-5 family accepts `reasoning_effort:
        // "minimal"`. Providers without `supportsReasoningEffort`
        // ignore it silently; the mapping just has to be in
        // place so those that do support it get the right wire
        // value.
        assert_eq!(reasoning_effort(ThinkingLevel::Minimal), Some("minimal"));
    }

    #[test]
    fn reasoning_effort_maps_from_thinking_level() {
        assert_eq!(reasoning_effort(ThinkingLevel::Off), None);
        assert_eq!(reasoning_effort(ThinkingLevel::Low), Some("low"));
        assert_eq!(reasoning_effort(ThinkingLevel::Medium), Some("medium"));
        assert_eq!(reasoning_effort(ThinkingLevel::High), Some("high"));
    }

    #[test]
    fn is_native_reasoning_compatibility_error_only_matches_typed_variant() {
        assert!(is_native_reasoning_compatibility_error(
            &ProviderError::NativeReasoningUnsupported("unknown field reasoning".into()),
        ));
        assert!(!is_native_reasoning_compatibility_error(
            &ProviderError::Http {
                status: 400,
                body: "unknown field reasoning_effort".into(),
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
    }

    #[test]
    fn classify_openai_http_error_upgrades_reasoning_compat_bodies() {
        let err = classify_openai_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            "unknown field reasoning_effort",
            None,
        );
        assert!(matches!(err, ProviderError::NativeReasoningUnsupported(_)));

        let err = classify_openai_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            "bad request: extra inputs are not permitted for reasoning",
            None,
        );
        assert!(matches!(err, ProviderError::NativeReasoningUnsupported(_)));

        let err = classify_openai_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            "missing required field messages",
            None,
        );
        assert!(matches!(err, ProviderError::Http { .. }));

        let err = classify_openai_http_error(reqwest::StatusCode::UNAUTHORIZED, "nope", None);
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn classify_openai_http_error_recognizes_ollama_leveled_think_rejection() {
        // Ollama reports non-thinking-capable models rejecting a
        // leveled `think` value with this exact body. Must upgrade
        // to NativeReasoningUnsupported so the caller retries with
        // `NoNativeFields`.
        let err = classify_openai_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"think value \"low\" is not supported for this model","type":"api_error"}}"#,
            None,
        );
        assert!(
            matches!(err, ProviderError::NativeReasoningUnsupported(_)),
            "expected NativeReasoningUnsupported, got {err:?}"
        );
    }

    #[test]
    fn classify_openai_http_error_recognizes_ollama_no_thinking_capability() {
        // Alternate Ollama wording for models lacking the
        // `thinking` capability entirely.
        let err = classify_openai_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"\"gemma3:1b\" does not support thinking","type":"api_error"}}"#,
            None,
        );
        assert!(
            matches!(err, ProviderError::NativeReasoningUnsupported(_)),
            "expected NativeReasoningUnsupported, got {err:?}"
        );
    }

    #[test]
    fn classify_openai_http_error_does_not_misclassify_unrelated_400s() {
        // Negative: a 400 body about a genuinely-unrelated field
        // must stay a plain Http error so the caller surfaces the
        // underlying mistake instead of retrying into a
        // reasoning-compat fallback.
        let err = classify_openai_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"missing required field \"messages\"","type":"api_error"}}"#,
            None,
        );
        assert!(
            matches!(err, ProviderError::Http { .. }),
            "expected Http {{ .. }}, got {err:?}"
        );
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
                NativeReasoningRequestStrategy::NestedReasoning,
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
