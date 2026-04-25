//! OpenRouter-specific helpers.
//!
//! OpenRouter is a single OpenAI-Compatible endpoint that
//! fronts many upstream providers (Anthropic, OpenAI, Google,
//! DeepSeek, Meta, …). Each upstream has its own quirks that
//! surface through the unified wire protocol — signed
//! thinking, encrypted reasoning wrappers, per-request cache
//! control, etc. This module centralizes three concerns that
//! apply to the shared `OpenAICompletions` provider only when
//! the target is OpenRouter:
//!
//! 1. **Capability mapping by upstream prefix.** An OpenRouter
//!    model id carries its upstream as a prefix
//!    (`anthropic/claude-sonnet-4`, `openai/o3`,
//!    `google/gemini-2.5-pro`). We infer the correct
//!    `ReplayCapabilities` + `ReasoningCapabilities` from that
//!    prefix alone and store them on `Model` at discovery time.
//! 2. **Anthropic `cache_control` insertion.** Upstreams whose
//!    native API supports prompt-cache pricing (currently just
//!    Anthropic) need an explicit marker on the last text part.
//! 3. **Provider-routing preferences.** When the user configures
//!    `OpenRouterRouting` via the catalog's compat blob, those
//!    preferences serialize into the top-level `provider` field
//!    on outbound requests.
//!
//! The logic lives here rather than inside `openai/` so the
//! OpenAI-compatible provider stays ignorant of OpenRouter;
//! the `OpenAIProvider` just consumes catalog entries whose
//! capabilities are already set correctly.

use anie_provider::{
    Model, ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode, ReplayCapabilities,
    ThinkingRequestMode,
};

/// True when `base_url` targets OpenRouter's API. Match is
/// substring-based so custom proxies / test fixtures that set
/// `https://openrouter.ai/api/v1` through a tunnel still match.
#[must_use]
pub fn is_openrouter_target(base_url: &str) -> bool {
    base_url.contains("openrouter.ai")
}

/// Upstream provider slug parsed from an OpenRouter model id.
/// Returns `None` for ids without a `/` separator.
fn upstream_prefix(model_id: &str) -> Option<&str> {
    model_id.split_once('/').map(|(upstream, _)| upstream)
}

/// True when `model_id` matches the OpenRouter convention for
/// an OpenAI reasoning upstream (`openai/o1`, `openai/o3`,
/// `openai/o4-mini`, `openai/gpt-5`, `openai/gpt-5-codex`, …).
fn is_openai_reasoning_upstream(model_id: &str) -> bool {
    let Some(rest) = model_id.strip_prefix("openai/") else {
        return false;
    };
    rest.starts_with('o') || rest.starts_with("gpt-5")
}

/// Compute per-model replay and reasoning capabilities for an
/// OpenRouter catalog entry based on its upstream prefix and
/// whether the upstream flagged reasoning support. The returned
/// pair goes directly onto the `Model` during discovery.
#[must_use]
pub fn openrouter_capabilities_for(
    model_id: &str,
    supports_reasoning: bool,
) -> (Option<ReplayCapabilities>, Option<ReasoningCapabilities>) {
    let upstream = upstream_prefix(model_id);

    let replay = match (upstream, supports_reasoning) {
        (Some("anthropic"), true) => Some(ReplayCapabilities {
            requires_thinking_signature: true,
            supports_redacted_thinking: false,
            supports_encrypted_reasoning: false,
            supports_reasoning_details_replay: false,
        }),
        (Some("openai"), true) if is_openai_reasoning_upstream(model_id) => {
            Some(ReplayCapabilities {
                requires_thinking_signature: false,
                supports_redacted_thinking: false,
                supports_encrypted_reasoning: false,
                supports_reasoning_details_replay: true,
            })
        }
        _ => None,
    };

    let reasoning = if supports_reasoning {
        Some(ReasoningCapabilities {
            control: Some(ReasoningControlMode::Native),
            output: Some(ReasoningOutputMode::Separated),
            tags: None,
            request_mode: Some(ThinkingRequestMode::NestedReasoning),
        })
    } else {
        None
    };

    (replay, reasoning)
}

/// Apply OpenRouter capability mapping to a discovered `Model`
/// in place. No-op when `model` does not target OpenRouter.
pub fn apply_openrouter_capabilities(model: &mut Model) {
    if !is_openrouter_target(&model.base_url) {
        return;
    }
    let (replay, reasoning) = openrouter_capabilities_for(&model.id, model.supports_reasoning);
    if replay.is_some() {
        model.replay_capabilities = replay;
    }
    if reasoning.is_some() {
        model.reasoning_capabilities = reasoning;
    }

    // OpenAI o-series + GPT-5 require `max_completion_tokens` on
    // the wire. OpenRouter normalizes this for intra-proxy use
    // but once a request hits the actual OpenAI endpoint it
    // 400s on the legacy name. Opt the compat flag in for
    // upstreams we know about.
    if is_openai_reasoning_upstream(&model.id) {
        let current_compat = match std::mem::take(&mut model.compat) {
            anie_provider::ModelCompat::OpenAICompletions(compat) => compat,
            anie_provider::ModelCompat::None => anie_provider::OpenAICompletionsCompat::default(),
        };
        model.compat =
            anie_provider::ModelCompat::OpenAICompletions(anie_provider::OpenAICompletionsCompat {
                max_tokens_field: Some(anie_provider::MaxTokensField::MaxCompletionTokens),
                ..current_compat
            });
    }
}

/// True when the outbound request body should carry an
/// Anthropic-flavored `cache_control: ephemeral` marker on the
/// last text part. Only applies when the model targets OpenRouter
/// AND the upstream is Anthropic; other upstreams either don't
/// recognize the marker or route through OpenRouter's own cache
/// layer.
#[must_use]
pub fn needs_anthropic_cache_control(model: &Model) -> bool {
    is_openrouter_target(&model.base_url) && upstream_prefix(&model.id) == Some("anthropic")
}

/// Decorate the last text part of the message list with an
/// Anthropic `cache_control: ephemeral` marker. Walks
/// back-to-front, locates the last message whose content is a
/// text string or a structured array containing a trailing text
/// part, and replaces that text part with a content-array entry
/// of the form:
///
/// ```json
/// { "type": "text", "text": "...", "cache_control": {"type": "ephemeral"} }
/// ```
///
/// No-op when the list has no text part. Idempotent: re-running
/// on an already-marked list produces the same output.
pub fn insert_anthropic_cache_control(messages: &mut [serde_json::Value]) {
    for message in messages.iter_mut().rev() {
        if try_mark_last_text_part(message) {
            return;
        }
    }
}

fn try_mark_last_text_part(message: &mut serde_json::Value) -> bool {
    let Some(obj) = message.as_object_mut() else {
        return false;
    };
    let Some(content) = obj.get_mut("content") else {
        return false;
    };

    if let Some(text) = content.as_str() {
        let owned = text.to_string();
        *content = serde_json::json!([
            {
                "type": "text",
                "text": owned,
                "cache_control": {"type": "ephemeral"},
            }
        ]);
        return true;
    }

    let Some(parts) = content.as_array_mut() else {
        return false;
    };
    for part in parts.iter_mut().rev() {
        let Some(part_obj) = part.as_object_mut() else {
            continue;
        };
        let is_text_part = part_obj
            .get("type")
            .and_then(|value| value.as_str())
            .map(|value| value.eq_ignore_ascii_case("text"))
            .unwrap_or_else(|| part_obj.contains_key("text"));
        if !is_text_part {
            continue;
        }
        part_obj.insert(
            "cache_control".to_string(),
            serde_json::json!({"type": "ephemeral"}),
        );
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_provider::{ApiKind, CostPerMillion, ModelCompat};

    fn openrouter_model(id: &str, supports_reasoning: bool) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            provider: "openrouter".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://openrouter.ai/api/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    #[test]
    fn openrouter_capabilities_anthropic_reasoning_sets_signature() {
        let (replay, reasoning) = openrouter_capabilities_for("anthropic/claude-sonnet-4", true);
        let replay = replay.expect("anthropic reasoning => replay caps");
        assert!(replay.requires_thinking_signature);
        assert!(!replay.supports_reasoning_details_replay);
        let reasoning = reasoning.expect("anthropic reasoning => reasoning caps");
        assert_eq!(reasoning.control, Some(ReasoningControlMode::Native));
        assert_eq!(
            reasoning.request_mode,
            Some(ThinkingRequestMode::NestedReasoning)
        );
    }

    #[test]
    fn openrouter_capabilities_openai_o_series_sets_reasoning_details_replay() {
        for id in ["openai/o1", "openai/o3", "openai/o4-mini", "openai/gpt-5"] {
            let (replay, _) = openrouter_capabilities_for(id, true);
            let replay = replay.unwrap_or_else(|| panic!("{id} => replay caps"));
            assert!(
                replay.supports_reasoning_details_replay,
                "{id} should require reasoning_details replay"
            );
            assert!(
                !replay.requires_thinking_signature,
                "{id} must not require thinking signature"
            );
        }
    }

    #[test]
    fn openrouter_capabilities_google_reasoning_sets_nested_reasoning() {
        let (replay, reasoning) = openrouter_capabilities_for("google/gemini-2.5-pro", true);
        // Google upstreams don't need replay capabilities today
        // (Gemini's thought_signature support lands with the
        // Google work), but they still get the nested reasoning
        // shape so OpenRouter sees an explicit effort.
        assert!(replay.is_none(), "no replay caps for google reasoning yet");
        let reasoning = reasoning.expect("google reasoning => reasoning caps");
        assert_eq!(
            reasoning.request_mode,
            Some(ThinkingRequestMode::NestedReasoning)
        );
    }

    #[test]
    fn openrouter_capabilities_non_reasoning_returns_none() {
        let (replay, reasoning) =
            openrouter_capabilities_for("meta-llama/llama-3.1-8b-instruct", false);
        assert!(replay.is_none());
        assert!(reasoning.is_none());

        // Even reasoning-capable upstreams return no caps when
        // the per-model flag is false (OpenRouter's catalog is
        // source-of-truth).
        let (replay, reasoning) = openrouter_capabilities_for("anthropic/claude-haiku-4", false);
        assert!(replay.is_none());
        assert!(reasoning.is_none());
    }

    #[test]
    fn apply_openrouter_capabilities_is_noop_for_non_openrouter_models() {
        let mut model = openrouter_model("anthropic/claude-sonnet-4", true);
        model.base_url = "https://api.openai.com/v1".into();
        apply_openrouter_capabilities(&mut model);
        assert!(model.replay_capabilities.is_none());
        assert!(model.reasoning_capabilities.is_none());
    }

    #[test]
    fn apply_openrouter_capabilities_populates_catalog_entry() {
        let mut model = openrouter_model("openai/o3", true);
        apply_openrouter_capabilities(&mut model);
        assert!(
            model
                .replay_capabilities
                .as_ref()
                .expect("replay caps")
                .supports_reasoning_details_replay
        );
        assert_eq!(
            model
                .reasoning_capabilities
                .as_ref()
                .expect("reasoning caps")
                .request_mode,
            Some(ThinkingRequestMode::NestedReasoning)
        );
    }

    #[test]
    fn apply_openrouter_capabilities_opts_openai_o_series_into_max_completion_tokens() {
        // Plan 01 PR A: o-series upstreams 400 on the legacy
        // `max_tokens` wire name. OpenRouter normalizes it
        // internally but once a request reaches the real
        // OpenAI endpoint the rejection surfaces. Opt-in via
        // the compat blob during discovery.
        use anie_provider::{MaxTokensField, ModelCompat as MC, OpenAICompletionsCompat};

        for id in ["openai/o1", "openai/o3", "openai/o4-mini", "openai/gpt-5"] {
            let mut model = openrouter_model(id, true);
            apply_openrouter_capabilities(&mut model);
            match &model.compat {
                MC::OpenAICompletions(OpenAICompletionsCompat {
                    max_tokens_field, ..
                }) => assert_eq!(
                    *max_tokens_field,
                    Some(MaxTokensField::MaxCompletionTokens),
                    "{id} should opt into max_completion_tokens",
                ),
                other => panic!("{id}: expected OpenAICompletions compat, got {other:?}"),
            }
        }
    }

    #[test]
    fn apply_openrouter_capabilities_leaves_non_openai_upstreams_on_legacy_field() {
        // Anthropic / Google / Meta upstreams still accept the
        // legacy name on OpenRouter — don't flip them.
        use anie_provider::ModelCompat as MC;

        for id in [
            "anthropic/claude-sonnet-4",
            "google/gemini-2.5-pro",
            "meta-llama/llama-3.1-8b-instruct",
        ] {
            let mut model = openrouter_model(id, true);
            apply_openrouter_capabilities(&mut model);
            match &model.compat {
                MC::None => {}
                MC::OpenAICompletions(compat) => {
                    assert!(
                        compat.max_tokens_field.is_none(),
                        "{id} should not opt into max_completion_tokens",
                    );
                }
            }
        }
    }

    #[test]
    fn needs_anthropic_cache_control_only_triggers_for_openrouter_anthropic_upstream() {
        assert!(needs_anthropic_cache_control(&openrouter_model(
            "anthropic/claude-sonnet-4",
            true,
        )));
        assert!(!needs_anthropic_cache_control(&openrouter_model(
            "openai/o3",
            true,
        )));

        let mut direct_anthropic = openrouter_model("claude-sonnet-4-6", true);
        direct_anthropic.base_url = "https://api.anthropic.com".into();
        direct_anthropic.provider = "anthropic".into();
        assert!(
            !needs_anthropic_cache_control(&direct_anthropic),
            "cache_control shouldn't be OR-inserted on direct Anthropic calls"
        );
    }

    #[test]
    fn insert_anthropic_cache_control_marks_last_text_part_in_array_content() {
        let mut messages = vec![
            serde_json::json!({
                "role": "system",
                "content": "you are helpful",
            }),
            serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "image", "image_url": {"url": "http://…"}},
                    {"type": "text", "text": "describe this"},
                ],
            }),
        ];
        insert_anthropic_cache_control(&mut messages);
        let last = &messages[1]["content"].as_array().expect("array")[1];
        assert_eq!(last["cache_control"]["type"], "ephemeral");
        // Earlier messages untouched.
        assert!(messages[0]["content"].is_string());
    }

    #[test]
    fn insert_anthropic_cache_control_upgrades_string_content_to_structured_array() {
        let mut messages = vec![serde_json::json!({
            "role": "user",
            "content": "one-shot question",
        })];
        insert_anthropic_cache_control(&mut messages);
        let parts = messages[0]["content"]
            .as_array()
            .expect("promoted to array");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "one-shot question");
        assert_eq!(parts[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn insert_anthropic_cache_control_is_noop_when_no_text_part_present() {
        let mut messages = vec![serde_json::json!({
            "role": "tool",
            "tool_call_id": "abc",
            "content": [],
        })];
        let before = messages.clone();
        insert_anthropic_cache_control(&mut messages);
        assert_eq!(messages, before);
    }
}
