//! Effective compaction-reserve calculation.
//!
//! `CompactionConfig::reserve_tokens` is configured as a flat
//! number (default 16,384), but the trigger threshold is
//! `context_window - reserve_tokens`. On small local-model
//! windows that flat default is broken: a 16K window minus a
//! 16K reserve saturates to threshold 0, which makes every
//! turn trigger compaction unconditionally. An 8K window goes
//! the same way.
//!
//! This module hosts the small clamp that lets the configured
//! reserve scale with the window without changing the
//! published `CompactionConfig` shape, so `anie-session`
//! stays unaware of context windows. Plan
//! `docs/midturn_compaction_2026-04-27/01_context_aware_reserve.md`.
//!
//! See `effective_reserve` for the formula.

/// Floor on the effective reserve. A pathological combination
/// (very small window plus a tiny configured reserve) must not
/// produce a reserve below this; otherwise the compaction
/// threshold ends up too close to the window itself and we
/// trigger compaction at the very last moment, when the
/// in-flight request may already have spilled over.
///
/// Made a constant rather than a config knob in PR A; PR B of
/// the plan exposes it as `[compaction] min_reserve_tokens`.
pub(crate) const DEFAULT_MIN_RESERVE_TOKENS: u64 = 1_024;

/// Quarter-window cap on the effective reserve. With this cap,
/// the resulting `threshold = window - reserve` lives at >= 75%
/// of the window — context fills before the trigger fires
/// rather than at startup.
const QUARTER_DIVISOR: u64 = 4;

/// Compute the effective reserve given the configured value
/// and the model's actual context window.
///
/// Three rules apply, in order:
///
/// 1. Cap at `window / 4`. The configured value is an upper
///    bound but cannot exceed 25% of the window.
/// 2. Floor at `min_reserve`. After the cap, if the value
///    fell below the floor, snap back up.
/// 3. Then a final clamp at `window` itself — pathological
///    inputs (`min_reserve > window`) shouldn't produce a
///    reserve larger than the window.
///
/// Examples for `min_reserve = 1024`:
///
/// | window  | configured | result | threshold |
/// |---------|------------|--------|-----------|
/// | 200,000 | 16,384     | 16,384 | 183,616   |
/// | 65,536  | 16,384     | 16,384 | 49,152    |
/// | 32,768  | 16,384     | 8,192  | 24,576    |
/// | 16,384  | 16,384     | 4,096  | 12,288    |
/// | 8,192   | 16,384     | 2,048  | 6,144     |
/// | 4,096   | 16,384     | 1,024  | 3,072     |
pub(crate) fn effective_reserve(window: u64, configured: u64, min_reserve: u64) -> u64 {
    let cap = window / QUARTER_DIVISOR;
    let capped = configured.min(cap);
    let floored = capped.max(min_reserve);
    floored.min(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_reserve_keeps_configured_when_under_quarter_window() {
        // 200K window, 16K configured: 16K is below 200K/4
        // = 50K, so the configured value passes through.
        assert_eq!(
            effective_reserve(200_000, 16_384, DEFAULT_MIN_RESERVE_TOKENS),
            16_384,
        );
        assert_eq!(
            effective_reserve(65_536, 16_384, DEFAULT_MIN_RESERVE_TOKENS),
            16_384,
        );
    }

    #[test]
    fn effective_reserve_clamps_to_quarter_window() {
        // Small windows: configured exceeds quarter, so the
        // quarter takes over. Threshold stays well below
        // window so context can actually fill.
        assert_eq!(
            effective_reserve(32_768, 16_384, DEFAULT_MIN_RESERVE_TOKENS),
            8_192, // 32_768 / 4
        );
        assert_eq!(
            effective_reserve(16_384, 16_384, DEFAULT_MIN_RESERVE_TOKENS),
            4_096, // 16_384 / 4
        );
        assert_eq!(
            effective_reserve(8_192, 16_384, DEFAULT_MIN_RESERVE_TOKENS),
            2_048, // 8_192 / 4
        );
    }

    #[test]
    fn effective_reserve_floors_at_min_reserve() {
        // A 4K window with configured 1K would clamp at
        // window/4 = 1024 — exactly the floor. A 4K window
        // with configured 100 would clamp the configured to
        // 100 first, then snap up to the floor.
        assert_eq!(
            effective_reserve(4_096, 100, DEFAULT_MIN_RESERVE_TOKENS),
            DEFAULT_MIN_RESERVE_TOKENS,
        );
        // 2K window: quarter is 512, but floor pulls back to
        // min_reserve. The final clamp keeps it at min_reserve
        // since 1024 < 2048 = window.
        assert_eq!(
            effective_reserve(2_048, 16_384, DEFAULT_MIN_RESERVE_TOKENS),
            DEFAULT_MIN_RESERVE_TOKENS,
        );
    }

    #[test]
    fn effective_reserve_handles_pathological_inputs() {
        // `min_reserve` larger than the window. The final
        // clamp keeps the result at `window` so we don't
        // produce a reserve larger than the window.
        assert_eq!(effective_reserve(512, 16_384, 4_096), 512);
        // `window = 0` should produce 0 (degenerate config —
        // but should not panic and should not produce a value
        // larger than the window).
        assert_eq!(effective_reserve(0, 16_384, DEFAULT_MIN_RESERVE_TOKENS), 0);
    }

    /// Property test (cheap version): for a representative
    /// matrix of windows and configured values, the resulting
    /// `threshold = window - effective_reserve` is non-zero
    /// (unless the window itself is below the floor, which is
    /// a configuration error).
    #[test]
    fn effective_reserve_keeps_threshold_positive_for_real_windows() {
        for window in [
            2_048, 4_096, 8_192, 16_384, 32_768, 65_536, 131_072, 200_000,
        ] {
            for configured in [1_024, 4_096, 16_384, 65_536] {
                let reserve = effective_reserve(window, configured, DEFAULT_MIN_RESERVE_TOKENS);
                let threshold = window.saturating_sub(reserve);
                assert!(
                    reserve <= window,
                    "reserve {reserve} > window {window} (configured {configured})",
                );
                if window > DEFAULT_MIN_RESERVE_TOKENS {
                    assert!(
                        threshold > 0,
                        "threshold zero for window={window}, configured={configured}, reserve={reserve}",
                    );
                }
            }
        }
    }
}
