//! Retry-config type and pure retry-decision helpers.
//!
//! The controller owns event emission and session mutation, but every
//! retry decision itself flows through `RetryPolicy::decide` so the
//! transient-retry, compaction-retry, and give-up paths share one
//! source of truth.

use anie_provider::ProviderError;

/// Retry knobs: max attempts, initial / ceiling delays, exponential
/// multiplier, and whether to apply a +/- 25% jitter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RetryConfig {
    pub max_retries: u32,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub backoff_multiplier: f64,
    pub jitter: bool,
}

/// Maximum retries for `RateLimited` errors specifically. Rate
/// limits almost always indicate a per-minute-or-longer cool-down
/// window; retrying 3× with sub-10-second backoff just burns more
/// of the user's rate budget and speeds up the next lockout. We
/// cap at one cool-down retry and surface the error if that also
/// fails, so the user can switch models / wait / top up credits
/// instead of watching the agent churn.
const MAX_RATE_LIMIT_RETRIES: u32 = 1;

/// Cap on retries for `ProviderError::ModelOutputMalformed`. A
/// fresh sample at the same context is usually a different
/// generation, so a retry has a real chance of succeeding —
/// but if the model keeps producing parse-failing output at
/// this context size, the underlying cause is context
/// pressure, not transient sampling. Two attempts strikes the
/// balance: one retry catches the random-bad-sample case;
/// stopping after that surfaces a clean error so the user can
/// react (lower context, switch models, wait for the planned
/// mid-turn compaction work to land).
const MAX_MODEL_OUTPUT_MALFORMED_RETRIES: u32 = 2;

/// Minimum delay (ms) to wait before retrying a rate-limited
/// request when the provider did not send a `Retry-After` header.
/// OpenRouter's `:free` tier and similar gated endpoints drop
/// 429s without a reset hint; waiting the usual 1 s just returns
/// another 429 immediately.
const RATE_LIMIT_FALLBACK_DELAY_MS: u64 = 15_000;

/// Floor (ms) for *any* rate-limit retry delay, including ones
/// computed from `Retry-After`. Guards against providers that
/// advertise tiny retry-after values for bursty workloads.
const RATE_LIMIT_MIN_DELAY_MS: u64 = 2_000;

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 1_000,
            max_delay_ms: 30_000,
            backoff_multiplier: 2.0,
            jitter: true,
        }
    }
}

/// What to do about a `ProviderError` that ended a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// Retry the same request after the given delay.
    Retry { attempt: u32, delay_ms: u64 },
    /// Compact the session and then retry once.
    Compact,
    /// Stop retrying.
    GiveUp { reason: GiveUpReason },
}

/// Why retrying stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GiveUpReason {
    /// Error is terminal.
    Terminal,
    /// The run already compacted once and still overflowed.
    AlreadyCompacted,
    /// The configured retry limit has been exhausted.
    AttemptsExhausted,
    /// Context overflow arrived after the per-turn compaction
    /// budget was exhausted. Surfaces an actionable message
    /// (smaller prompt / larger window / raise
    /// `[compaction] max_per_turn`) rather than letting the
    /// turn loop on serial overflow→compact→overflow grinds.
    /// PR 8.2 of `docs/midturn_compaction_2026-04-27/`.
    CompactionBudgetExhausted,
}

/// Pure retry-decision helper.
pub(crate) struct RetryPolicy<'a> {
    pub(crate) config: &'a RetryConfig,
}

impl<'a> RetryPolicy<'a> {
    /// Decide what to do after `attempt` transient retries have
    /// already happened. `already_compacted` indicates whether
    /// this run has already been through the overflow-compaction
    /// path. `compaction_budget_remaining` is the number of
    /// compactions still allowed in the current user turn — when
    /// zero, a fresh `ContextOverflow` gives up with
    /// `GiveUpReason::CompactionBudgetExhausted` rather than
    /// triggering yet another compact-and-retry cycle. Plan
    /// `docs/midturn_compaction_2026-04-27/02_per_turn_compaction_budget.md`.
    pub(crate) fn decide(
        &self,
        error: &ProviderError,
        attempt: u32,
        already_compacted: bool,
        compaction_budget_remaining: u32,
    ) -> RetryDecision {
        match error {
            ProviderError::ContextOverflow(_) => {
                if already_compacted {
                    RetryDecision::GiveUp {
                        reason: GiveUpReason::AlreadyCompacted,
                    }
                } else if compaction_budget_remaining == 0 {
                    RetryDecision::GiveUp {
                        reason: GiveUpReason::CompactionBudgetExhausted,
                    }
                } else {
                    RetryDecision::Compact
                }
            }
            ProviderError::Auth(_)
            | ProviderError::RequestBuild(_)
            | ProviderError::ToolCallMalformed(_)
            | ProviderError::NativeReasoningUnsupported(_)
            | ProviderError::UnsupportedStreamFeature(_)
            | ProviderError::ReplayFidelity { .. }
            | ProviderError::FeatureUnsupported(_)
            // `EmptyAssistantResponse` surfaces when the model produced
            // no text and no tool call (commonly: only tagged reasoning
            // came back). Retrying against the same context usually
            // reproduces the same shape — we'd just replay the same
            // thinking block N times before giving up. Treat it as
            // terminal so the user sees one clean error and can adjust
            // the prompt or swap models.
            | ProviderError::EmptyAssistantResponse
            // `ResponseTruncated` is `finish_reason: "length"` with
            // nothing visible yet. Same retry logic as
            // `EmptyAssistantResponse` — the same prompt at the
            // same `max_tokens` produces the same truncation — but
            // the error message surfaces a more accurate fix.
            | ProviderError::ResponseTruncated
            // `ModelLoadResources` means Ollama refused to load the
            // model with the requested `num_ctx` because it doesn't
            // fit in available memory. The provider impl
            // (`OllamaChatProvider::stream` after PR 2 of the
            // load-failure plan) does one same-request retry with
            // a halved `num_ctx` before this variant reaches the
            // controller — by the time we see it here, halving has
            // already been tried. Outer retry would just attempt
            // the same too-large allocation again, so terminate
            // and let the user act on the actionable
            // `/context-length` suggestion the variant carries.
            | ProviderError::ModelLoadResources { .. } => RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            },
            ProviderError::RateLimited { .. } => {
                // Rate limits get a dedicated, stricter cap — see
                // `MAX_RATE_LIMIT_RETRIES`. Also bounded by the
                // user-configured `max_retries` so tests / rpc
                // clients that disable retries entirely still work.
                let rate_limit_cap = MAX_RATE_LIMIT_RETRIES.min(self.config.max_retries);
                if attempt >= rate_limit_cap {
                    RetryDecision::GiveUp {
                        reason: GiveUpReason::AttemptsExhausted,
                    }
                } else {
                    RetryDecision::Retry {
                        attempt: attempt + 1,
                        delay_ms: self.delay_for(error, attempt + 1),
                    }
                }
            }
            ProviderError::Transport(_)
            | ProviderError::InvalidStreamJson(_)
            | ProviderError::MalformedStreamEvent(_) => {
                if attempt >= self.config.max_retries {
                    RetryDecision::GiveUp {
                        reason: GiveUpReason::AttemptsExhausted,
                    }
                } else {
                    RetryDecision::Retry {
                        attempt: attempt + 1,
                        delay_ms: self.delay_for(error, attempt + 1),
                    }
                }
            }
            ProviderError::ModelOutputMalformed(_) => {
                // The provider rejected the model's output (Ollama's
                // XML/JSON parser failed on a Qwen-family
                // `<tool_call>` block, etc.). A fresh sample at the
                // same context tends to produce different tokens —
                // retry, but with a tighter cap than the transient
                // network errors above. If the model keeps emitting
                // bad output at this context size, the underlying
                // cause is usually context pressure, not transient
                // sampling; the answer is mid-turn compaction (see
                // `docs/midturn_compaction_2026-04-27/`), not more
                // retries.
                let cap = MAX_MODEL_OUTPUT_MALFORMED_RETRIES.min(self.config.max_retries);
                if attempt >= cap {
                    RetryDecision::GiveUp {
                        reason: GiveUpReason::AttemptsExhausted,
                    }
                } else {
                    RetryDecision::Retry {
                        attempt: attempt + 1,
                        delay_ms: self.delay_for(error, attempt + 1),
                    }
                }
            }
            ProviderError::Http { status, .. } => {
                if matches!(status, 429 | 500 | 502 | 503 | 529)
                    && attempt < self.config.max_retries
                {
                    RetryDecision::Retry {
                        attempt: attempt + 1,
                        delay_ms: self.delay_for(error, attempt + 1),
                    }
                } else {
                    RetryDecision::GiveUp {
                        reason: if attempt >= self.config.max_retries {
                            GiveUpReason::AttemptsExhausted
                        } else {
                            GiveUpReason::Terminal
                        },
                    }
                }
            }
        }
    }

    /// Compute the retry delay once a retry decision has been made.
    pub(crate) fn delay_for(&self, error: &ProviderError, attempt: u32) -> u64 {
        retry_delay_ms(self.config, error, attempt)
    }
}

/// Compute the delay before the next retry attempt.
///
/// Prefers the provider's `Retry-After` header (via
/// `ProviderError::retry_after_ms`) when present. Otherwise applies
/// exponential backoff from `initial_delay_ms` with
/// `backoff_multiplier ^ (attempt - 1)`. Clamps to
/// `max_delay_ms`. Optionally adds +/- 25% jitter.
pub(crate) fn retry_delay_ms(
    config: &RetryConfig,
    error: &ProviderError,
    retry_attempt: u32,
) -> u64 {
    let base_delay = match error {
        // Rate-limit errors get a dedicated delay schedule. If the
        // provider sent `Retry-After`, respect it (but enforce a
        // floor so tiny values don't trigger immediate re-attempts).
        // If they didn't — as OpenRouter's `:free` tier does —
        // assume a minute-scale cool-down and wait 15 s, so a
        // follow-up retry doesn't just burn more of the budget.
        ProviderError::RateLimited { retry_after_ms } => retry_after_ms
            .unwrap_or(RATE_LIMIT_FALLBACK_DELAY_MS)
            .max(RATE_LIMIT_MIN_DELAY_MS),
        _ => {
            if let Some(retry_after_ms) = error.retry_after_ms() {
                retry_after_ms
            } else {
                let exponent = retry_attempt.saturating_sub(1);
                let mut delay = config.initial_delay_ms as f64;
                for _ in 0..exponent {
                    delay *= config.backoff_multiplier;
                }
                delay as u64
            }
        }
    };
    let clamped = base_delay.min(config.max_delay_ms);
    if !config.jitter {
        return clamped;
    }

    let jitter = (clamped as f64 * 0.25) as u64;
    if jitter == 0 {
        return clamped;
    }
    let offset = rand::random::<u64>() % (jitter * 2 + 1);
    clamped.saturating_sub(jitter) + offset
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default per-turn compaction-budget value used by the
    /// existing tests: high enough that no test triggers the
    /// `CompactionBudgetExhausted` give-up path. PR 8.2 of
    /// `docs/midturn_compaction_2026-04-27/` introduced the
    /// fourth `decide` argument; tests that exercise the
    /// budget path itself pass `0` explicitly.
    const NOT_EXHAUSTED: u32 = u32::MAX;

    fn deterministic_policy(config: RetryConfig) -> RetryPolicy<'static> {
        let config = Box::leak(Box::new(config));
        RetryPolicy { config }
    }

    fn deterministic_config() -> RetryConfig {
        RetryConfig {
            jitter: false,
            ..RetryConfig::default()
        }
    }

    #[test]
    fn auth_error_gives_up_immediately() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::Auth("bad key".into()),
                0,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn replay_fidelity_gives_up_immediately() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ReplayFidelity {
                    provider_hint: "anthropic",
                    detail: "thinking.signature".into(),
                },
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn feature_unsupported_gives_up_immediately() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::FeatureUnsupported("x".into()),
                0,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn unsupported_stream_feature_gives_up_immediately() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::UnsupportedStreamFeature("server_tool_use".into()),
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn rate_limit_with_retry_after_waits_the_advertised_delay() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::RateLimited {
                    retry_after_ms: Some(7_000),
                },
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::Retry {
                attempt: 1,
                delay_ms: 7_000,
            }
        );
    }

    #[test]
    fn rate_limit_without_retry_after_uses_fallback_floor_not_initial_delay() {
        // Regression: OpenRouter's `:free` tier returns 429 with
        // no Retry-After. Using the default 1 s initial_delay_ms
        // just hammers the provider. Verify we fall back to the
        // rate-limit-specific 15 s delay instead.
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::RateLimited {
                    retry_after_ms: None,
                },
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::Retry {
                attempt: 1,
                delay_ms: RATE_LIMIT_FALLBACK_DELAY_MS,
            }
        );
    }

    #[test]
    fn rate_limit_with_tiny_retry_after_is_floored() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::RateLimited {
                    retry_after_ms: Some(200),
                },
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::Retry {
                attempt: 1,
                delay_ms: RATE_LIMIT_MIN_DELAY_MS,
            }
        );
    }

    #[test]
    fn rate_limit_gives_up_after_single_cool_down_attempt() {
        // Regression: retrying 3× on 429 just burns through the
        // per-minute budget on free-tier endpoints. Cap at a
        // single cool-down retry.
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::RateLimited {
                    retry_after_ms: None,
                },
                MAX_RATE_LIMIT_RETRIES,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::AttemptsExhausted,
            }
        );
    }

    #[test]
    fn context_overflow_triggers_compact_on_first_attempt() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ContextOverflow("too many tokens".into()),
                0,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::Compact
        );
    }

    /// PR 8.2 of `docs/midturn_compaction_2026-04-27/`. When
    /// the per-turn compaction budget is exhausted (zero
    /// remaining) and a fresh `ContextOverflow` arrives, the
    /// retry policy gives up with
    /// `CompactionBudgetExhausted` rather than triggering yet
    /// another compact-and-retry cycle. This is the
    /// safety-net that breaks compaction storms on small
    /// local-model windows.
    #[test]
    fn retry_policy_gives_up_when_budget_exhausted() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ContextOverflow("budget exhausted".into()),
                0,
                false, // not yet compacted in this turn
                0,     // but the per-turn budget is gone
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::CompactionBudgetExhausted,
            },
            "with budget=0, ContextOverflow must surface as CompactionBudgetExhausted",
        );
    }

    /// PR 8.2: the budget gate only fires when both
    /// conditions hold (`already_compacted == false` AND
    /// `budget_remaining == 0`). With at least one slot
    /// remaining we still take the Compact path — i.e. the
    /// budget is a ceiling, not a precondition.
    #[test]
    fn retry_policy_still_compacts_when_budget_remains() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ContextOverflow("first time".into()),
                0,
                false,
                1, // one slot still available
            ),
            RetryDecision::Compact,
        );
    }

    /// PR 8.2: `already_compacted=true` takes precedence over
    /// the budget check (gives up with `AlreadyCompacted`,
    /// not `CompactionBudgetExhausted`). This pins the
    /// existing precedence rule — the per-run already-compacted
    /// guard is still authoritative for retry continuations
    /// against a transient error after a reactive compact.
    #[test]
    fn retry_policy_already_compacted_outranks_budget_exhausted() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ContextOverflow("post-compact still over".into()),
                0,
                true, // run already compacted
                0,    // budget also exhausted, but that's fine
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::AlreadyCompacted,
            },
        );
    }

    #[test]
    fn context_overflow_gives_up_if_already_compacted() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ContextOverflow("still too many tokens".into()),
                0,
                true,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::AlreadyCompacted,
            }
        );
    }

    #[test]
    fn http_5xx_retries_up_to_limit() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::Http {
                    status: 503,
                    body: "unavailable".into(),
                },
                1,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::Retry {
                attempt: 2,
                delay_ms: 2_000,
            }
        );
        assert_eq!(
            policy.decide(
                &ProviderError::Http {
                    status: 503,
                    body: "still unavailable".into(),
                },
                3,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::AttemptsExhausted,
            }
        );
    }

    #[test]
    fn http_4xx_gives_up() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::Http {
                    status: 404,
                    body: "missing".into(),
                },
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn response_truncated_gives_up_immediately() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(&ProviderError::ResponseTruncated, 0, false, NOT_EXHAUSTED),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn empty_assistant_response_gives_up_immediately() {
        // Regression: qwen3.5:9b and other tagged-reasoning models
        // emit `<think>...</think>` with no trailing text. That
        // surfaces as `EmptyAssistantResponse`. Retrying against
        // the same context reproduces the same thinking block
        // verbatim, so the user experiences the same output being
        // re-streamed N times before the retry limit is hit. Treat
        // this as terminal so the failure is reported once.
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::EmptyAssistantResponse,
                0,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
        // Still terminal even after partial retries, in case the
        // classification was reached via a non-empty path first.
        assert_eq!(
            policy.decide(
                &ProviderError::EmptyAssistantResponse,
                2,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn stream_error_retries_limited_times() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::MalformedStreamEvent("socket dropped".into()),
                2,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::Retry {
                attempt: 3,
                delay_ms: 4_000,
            }
        );
        assert_eq!(
            policy.decide(
                &ProviderError::MalformedStreamEvent("socket dropped".into()),
                3,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::AttemptsExhausted,
            }
        );
    }

    #[test]
    fn tool_call_malformed_gives_up_as_terminal() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ToolCallMalformed("bad json".into()),
                0,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn native_reasoning_unsupported_gives_up_as_terminal() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::NativeReasoningUnsupported("unsupported".into()),
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn retry_policy_decide_classifies_model_load_resources_as_terminal() {
        // The provider impl (post-PR 2) does one halved-num_ctx
        // retry before the variant ever reaches the controller.
        // By the time `decide` sees it, halving has already been
        // tried and failed. Outer retry would just repeat the
        // same too-large allocation, so terminate and let the
        // user act on the variant's `/context-length` suggestion.
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ModelLoadResources {
                    body: "model requires more system memory".into(),
                    suggested_num_ctx: 16_384,
                },
                0,
                false,
                NOT_EXHAUSTED,
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    /// `ModelOutputMalformed` is auto-retryable up to its own
    /// cap. First and second attempts retry; the third hits the
    /// cap and gives up.
    #[test]
    fn retry_policy_decide_retries_model_output_malformed_under_cap() {
        let policy = deterministic_policy(deterministic_config());
        let error = ProviderError::ModelOutputMalformed(
            "xml syntax error on line 5: unexpected EOF".into(),
        );
        // Attempt 0 → retry.
        assert!(matches!(
            policy.decide(&error, 0, false, NOT_EXHAUSTED),
            RetryDecision::Retry { attempt: 1, .. }
        ));
        // Attempt 1 → retry (still under MAX_MODEL_OUTPUT_MALFORMED_RETRIES = 2).
        assert!(matches!(
            policy.decide(&error, 1, false, NOT_EXHAUSTED),
            RetryDecision::Retry { attempt: 2, .. }
        ));
    }

    /// At the cap, give up cleanly so the user sees the
    /// rendered `ModelOutputMalformed` message rather than a
    /// silent retry storm.
    #[test]
    fn retry_policy_decide_gives_up_on_model_output_malformed_at_cap() {
        let policy = deterministic_policy(deterministic_config());
        let error = ProviderError::ModelOutputMalformed("xml: unexpected eof".into());
        assert_eq!(
            policy.decide(
                &error,
                MAX_MODEL_OUTPUT_MALFORMED_RETRIES,
                false,
                NOT_EXHAUSTED
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::AttemptsExhausted,
            }
        );
    }

    #[test]
    fn retry_delay_prefers_retry_after_header() {
        let config = deterministic_config();
        let error = ProviderError::RateLimited {
            retry_after_ms: Some(7_000),
        };
        assert_eq!(retry_delay_ms(&config, &error, 1), 7_000);
    }

    #[test]
    fn retry_delay_for_rate_limit_without_retry_after_uses_fallback() {
        let config = deterministic_config();
        let error = ProviderError::RateLimited {
            retry_after_ms: None,
        };
        assert_eq!(
            retry_delay_ms(&config, &error, 1),
            RATE_LIMIT_FALLBACK_DELAY_MS,
        );
    }

    #[test]
    fn retry_delay_uses_exponential_backoff() {
        let config = deterministic_config();
        let error = ProviderError::MalformedStreamEvent("socket dropped".into());
        assert_eq!(retry_delay_ms(&config, &error, 1), 1_000);
        assert_eq!(retry_delay_ms(&config, &error, 2), 2_000);
        assert_eq!(retry_delay_ms(&config, &error, 3), 4_000);
    }
}
