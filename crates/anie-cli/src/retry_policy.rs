//! Retry-config type and backoff-delay calculation.
//!
//! The full retry-decision logic (transient retry, overflow-then-
//! compact retry, give up) still lives in the controller event loop
//! — it's interleaved with event emission and session mutation.
//! This module currently owns only the pure pieces: the policy
//! knobs and the delay calculator.

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
