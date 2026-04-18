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
    /// Send nested `reasoning: { effort: "..." }`.
    /// LM Studio's proprietary shape.
    LmStudioNestedReasoning,
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

/// Extract a thinking/reasoning delta from a streamed chat-completion
/// `delta` object. Returns the first non-empty value found in any of
/// `reasoning`, `reasoning_content`, or `thinking`.
pub(super) fn native_reasoning_delta(delta: &serde_json::Value) -> Option<String> {
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
