//! One-shot pre-loop decomposition. PR 4 of
//! `docs/rlm_subagents_2026-05-01/`.
//!
//! When `ANIE_DECOMPOSE=1` is set in `--harness-mode=rlm`,
//! the controller runs a single LLM call BEFORE the main
//! agent loop starts. That call asks the model to break the
//! user's task into 3-7 focused sub-tasks. The result is
//! injected into the agent's initial context as a
//! `<system-reminder source="decompose">` user message, so
//! the model sees a plan it produced before doing any work.
//!
//! The decomposition is best-effort. If the call fails (timeout,
//! provider error, empty output), the controller proceeds
//! without a plan rather than blocking the user's prompt.
//! The same applies to obviously-low-value cases — the
//! model is asked to return an empty list when the task is
//! trivial enough not to need a plan, and we skip injection
//! when that happens.

#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;
use std::time::Duration;

use anie_protocol::{ContentBlock, Message, UserMessage, now_millis};
use anie_provider::{
    LlmContext, ProviderEvent, ProviderRegistry, RequestOptionsResolver, StreamOptions,
    ThinkingLevel,
};
use futures::StreamExt;
use tracing::{debug, info, warn};

/// System prompt for the one-shot decompose call. Tight on
/// purpose — the model gets the user's task as the only
/// user-message content; this prompt sets format expectations.
///
/// Tuning lessons baked in (from comprehensive smoke 2026-05-02):
/// - Old prompt said "independently-solvable" which actively
///   pushed the model to flatten real dependencies. New prompt
///   invites `(depends on N)` markers when sub-tasks have
///   genuine prerequisites.
/// - Old NO_PLAN_NEEDED guidance was vague ("trivial enough
///   that a plan would add no value"). New version names the
///   concrete cases: single-fact answers, single-computation
///   answers.
const DECOMPOSE_SYSTEM_PROMPT: &str = "You are a planning assistant. Given a user's task, break it into 3-7 focused sub-tasks. Output as a numbered list, one sub-task per line, no other prose. Each sub-task should be small enough that a sub-agent could complete it in one focused pass.\n\nIf a sub-task depends on another, mark the dependency by appending `(depends on N)` to the sub-task line, where N is the prior sub-task number. Use `(depends on 2, 3)` for multiple. Sub-tasks WITHOUT a marker are assumed independent — and may be executed concurrently. Mark dependencies accurately: don't pretend everything is independent when it isn't, and don't invent dependencies where none exist.\n\nIf the user's task is a single-fact lookup, a single computation (e.g. `what is 7 * 8?`), or otherwise too trivial to benefit from a plan, output the single line: NO_PLAN_NEEDED. Don't produce a 1-sub-task plan — that adds noise without value.";

/// Cap on the decompose call's wall-clock. Plans should be
/// quick — a slow decompose just adds latency before the user
/// sees anything happen.
const DECOMPOSE_TIMEOUT_SECS: u64 = 30;

/// Cap on output tokens for the plan. 3-7 short lines fit in
/// well under this; a runaway model can't blow up the
/// pre-loop budget.
const DECOMPOSE_MAX_TOKENS: u64 = 512;

/// Sentinel returned by the model when no plan is needed.
const NO_PLAN_SENTINEL: &str = "NO_PLAN_NEEDED";

/// Runs the one-shot decompose call.
pub(crate) struct Decomposer {
    provider_registry: Arc<ProviderRegistry>,
    model: anie_provider::Model,
    request_options_resolver: Arc<dyn RequestOptionsResolver>,
    num_ctx_override: Option<u64>,
}

impl Decomposer {
    pub(crate) fn new(
        provider_registry: Arc<ProviderRegistry>,
        model: anie_provider::Model,
        request_options_resolver: Arc<dyn RequestOptionsResolver>,
        num_ctx_override: Option<u64>,
    ) -> Self {
        Self {
            provider_registry,
            model,
            request_options_resolver,
            num_ctx_override,
        }
    }

    /// Run the decomposition. Returns `Some(plan_text)` when
    /// the model produced a non-empty, non-sentinel plan;
    /// `None` for "no plan needed" or any failure mode (the
    /// caller proceeds without a plan).
    pub(crate) async fn decompose(&self, user_task: &str) -> Option<String> {
        if user_task.trim().is_empty() {
            return None;
        }
        let prompt_message = Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: user_task.to_string(),
            }],
            timestamp: now_millis(),
        });
        let prompt_slice = std::slice::from_ref(&prompt_message);

        let resolved = match self
            .request_options_resolver
            .resolve(&self.model, prompt_slice)
            .await
        {
            Ok(r) => r,
            Err(error) => {
                warn!(%error, "decompose: resolver failed; skipping plan");
                return None;
            }
        };
        let mut model = self.model.clone();
        if let Some(base_url) = resolved.base_url_override {
            model.base_url = base_url;
        }

        let Some(provider) = self.provider_registry.get(&model.api) else {
            warn!(api = ?model.api, "decompose: no provider for model; skipping plan");
            return None;
        };

        let llm_context = LlmContext {
            system_prompt: DECOMPOSE_SYSTEM_PROMPT.to_string(),
            messages: provider.convert_messages(prompt_slice),
            tools: Vec::new(),
        };
        let options = StreamOptions {
            api_key: resolved.api_key,
            headers: resolved.headers,
            num_ctx_override: self.num_ctx_override,
            thinking: ThinkingLevel::Off,
            max_tokens: Some(DECOMPOSE_MAX_TOKENS),
            ..StreamOptions::default()
        };

        let stream_result = provider.stream(&model, llm_context, options);
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(error) => {
                warn!(%error, "decompose: stream init failed; skipping plan");
                return None;
            }
        };
        let collect_fut = async {
            let mut buf = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    Ok(ProviderEvent::TextDelta(text)) => buf.push_str(&text),
                    Ok(ProviderEvent::Done(assistant)) => {
                        if buf.is_empty() {
                            for block in &assistant.content {
                                if let ContentBlock::Text { text } = block {
                                    buf.push_str(text);
                                }
                            }
                        }
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => return Err(format!("stream: {e}")),
                }
            }
            Ok::<String, String>(buf)
        };
        let raw = match tokio::time::timeout(
            Duration::from_secs(DECOMPOSE_TIMEOUT_SECS),
            collect_fut,
        )
        .await
        {
            Ok(Ok(text)) => text,
            Ok(Err(error)) => {
                warn!(%error, "decompose: stream error; skipping plan");
                return None;
            }
            Err(_) => {
                warn!(timeout_secs = DECOMPOSE_TIMEOUT_SECS, "decompose: timed out; skipping plan");
                return None;
            }
        };

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            debug!("decompose: empty response; skipping plan");
            return None;
        }
        if trimmed.contains(NO_PLAN_SENTINEL) {
            debug!("decompose: model returned NO_PLAN_NEEDED; skipping plan");
            return None;
        }
        info!(
            target: "anie_cli::decompose",
            plan_chars = trimmed.chars().count(),
            plan_lines = trimmed.lines().count(),
            "decompose plan generated"
        );
        info!(
            target: "anie_cli::decompose",
            plan = %trimmed,
            "decompose plan content"
        );
        Some(trimmed.to_string())
    }
}

/// Wrap a plan in the system-reminder framing the harness
/// uses elsewhere (skill loads, ledger, loop warnings).
/// Same channel = same model behavior (treated as injected
/// guidance, not identity).
pub(crate) fn render_plan_as_system_reminder(plan: &str) -> String {
    format!(
        "<system-reminder source=\"decompose\">\nPLAN (proposed sub-tasks for the request you just received):\n\n{plan}\n\nUse this as a guide. Each sub-task can be tackled separately — consider using the `recurse` tool when one needs its own focused sub-agent.\n</system-reminder>"
    )
}

/// `true` when `ANIE_DECOMPOSE=1` (or `true` / `yes`) is set.
pub(crate) fn decompose_env_enabled() -> bool {
    matches!(
        std::env::var("ANIE_DECOMPOSE").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_plan_wraps_in_system_reminder_tags() {
        let plan = "1. First\n2. Second";
        let rendered = render_plan_as_system_reminder(plan);
        assert!(
            rendered.starts_with("<system-reminder source=\"decompose\">"),
            "{rendered}"
        );
        assert!(rendered.contains("1. First"), "{rendered}");
        assert!(rendered.contains("2. Second"), "{rendered}");
        assert!(rendered.contains("recurse"), "{rendered}");
        assert!(rendered.ends_with("</system-reminder>"), "{rendered}");
    }

    #[test]
    fn decompose_env_enabled_recognises_truthy_values() {
        let cases = [("1", true), ("true", true), ("yes", true), ("0", false), ("", false)];
        for (value, expected) in cases {
            // Using temp_env::with_var here would be cleanest;
            // for a unit test we set/unset directly. The test
            // suite serializes ANIE_DECOMPOSE manipulation by
            // running these cases sequentially within one test.
            // SAFETY: tests in this crate run single-threaded
            // by default for env-mutation tests; the gate
            // matters per-iteration only.
            // SAFETY: setting/removing env vars is unsafe in
            // multi-threaded contexts. Tests in this crate run
            // single-threaded for env mutations.
            unsafe {
                std::env::set_var("ANIE_DECOMPOSE", value);
            }
            assert_eq!(decompose_env_enabled(), expected, "value={value}");
        }
        unsafe {
            std::env::remove_var("ANIE_DECOMPOSE");
        }
    }
}
