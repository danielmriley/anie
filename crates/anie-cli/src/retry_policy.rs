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
}

/// Pure retry-decision helper.
pub(crate) struct RetryPolicy<'a> {
    pub(crate) config: &'a RetryConfig,
}

impl<'a> RetryPolicy<'a> {
    /// Decide what to do after `attempt` transient retries have
    /// already happened. `already_compacted` indicates whether this
    /// run has already been through the overflow-compaction path.
    pub(crate) fn decide(
        &self,
        error: &ProviderError,
        attempt: u32,
        already_compacted: bool,
    ) -> RetryDecision {
        match error {
            ProviderError::ContextOverflow(_) => {
                if already_compacted {
                    RetryDecision::GiveUp {
                        reason: GiveUpReason::AlreadyCompacted,
                    }
                } else {
                    RetryDecision::Compact
                }
            }
            ProviderError::Auth(_)
            | ProviderError::RequestBuild(_)
            | ProviderError::ToolCallMalformed(_)
            | ProviderError::NativeReasoningUnsupported(_)
            | ProviderError::UnsupportedStreamFeature(_) => RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            },
            ProviderError::RateLimited { .. }
            | ProviderError::Transport(_)
            | ProviderError::EmptyAssistantResponse
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
    let base_delay = if let Some(retry_after_ms) = error.retry_after_ms() {
        retry_after_ms
    } else {
        let exponent = retry_attempt.saturating_sub(1);
        let mut delay = config.initial_delay_ms as f64;
        for _ in 0..exponent {
            delay *= config.backoff_multiplier;
        }
        delay as u64
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
            policy.decide(&ProviderError::Auth("bad key".into()), 0, false),
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
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
            }
        );
    }

    #[test]
    fn rate_limit_returns_retry_with_backoff() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::RateLimited {
                    retry_after_ms: Some(7_000),
                },
                0,
                false,
            ),
            RetryDecision::Retry {
                attempt: 1,
                delay_ms: 7_000,
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
                false
            ),
            RetryDecision::Compact
        );
    }

    #[test]
    fn context_overflow_gives_up_if_already_compacted() {
        let policy = deterministic_policy(deterministic_config());
        assert_eq!(
            policy.decide(
                &ProviderError::ContextOverflow("still too many tokens".into()),
                0,
                true
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
                false
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
                false
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
                false
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
            ),
            RetryDecision::GiveUp {
                reason: GiveUpReason::Terminal,
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
    fn retry_delay_uses_exponential_backoff() {
        let config = deterministic_config();
        let error = ProviderError::MalformedStreamEvent("socket dropped".into());
        assert_eq!(retry_delay_ms(&config, &error, 1), 1_000);
        assert_eq!(retry_delay_ms(&config, &error, 2), 2_000);
        assert_eq!(retry_delay_ms(&config, &error, 3), 4_000);
    }
}
