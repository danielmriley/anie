//! Recurse-depth observability. PR 1 of
//! `docs/rlm_subagents_2026-05-01/`.
//!
//! Tracks the depth at which `recurse` tool calls fire and
//! surfaces a `tracing::info!` log + `SystemMessage` event
//! when depth crosses a configurable threshold. Mirrors the
//! `failure_loop` pattern: observability-only, never aborts,
//! throttles warnings (one per `(scope_kind, depth)` pair per
//! detector lifetime).
//!
//! The depth itself is already plumbed: `RecurseTool` carries
//! a `depth: u8` field and includes it in its `ToolResult.details`
//! payload. This detector reads `details["depth"]` after the
//! tool finishes and decides whether to fire a warning.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;

/// Default depth at which the warning fires. The controller
/// can override via `ANIE_RECURSE_DEPTH_WARN_AT`.
pub const DEFAULT_RECURSE_DEPTH_WARN_AT: u32 = 5;

/// Tracks recurse-tool depth observations to drive a single
/// warning per `(scope_kind, depth)` pair. Wrapped in a Mutex
/// by callers; concurrent `observe` calls (parallel tool
/// execution) may interleave but the only side effect is
/// possible duplicate warnings, which the throttle catches.
#[derive(Debug)]
pub(crate) struct RecurseDepthDetector {
    threshold: u32,
    /// `(scope_kind, depth)` pairs we've already warned
    /// about. Keeps the warning surface bounded — long
    /// recursive chains don't spam.
    warned: HashSet<(String, u32)>,
}

impl RecurseDepthDetector {
    /// Build a new detector. `threshold` is the recurse depth
    /// at which a warning fires (inclusive). A threshold of
    /// `0` would warn on every top-level recurse (silly); the
    /// controller is expected to provide a sane value.
    pub(crate) fn new(threshold: u32) -> Self {
        Self {
            threshold,
            warned: HashSet::new(),
        }
    }

    /// Record a recurse-tool result's depth + scope kind.
    /// Returns `Some(depth)` when the threshold is just
    /// crossed for a new `(scope_kind, depth)` pair, `None`
    /// otherwise. The caller turns the `Some` into a
    /// `tracing::info!` log + `SystemMessage` event for the
    /// transcript.
    pub(crate) fn observe(&mut self, scope_kind: &str, depth: u32) -> Option<u32> {
        if depth < self.threshold {
            return None;
        }
        let key = (scope_kind.to_string(), depth);
        if self.warned.contains(&key) {
            return None;
        }
        self.warned.insert(key);
        Some(depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_fires_at_threshold() {
        let mut det = RecurseDepthDetector::new(5);
        assert_eq!(det.observe("message_grep", 4), None);
        assert_eq!(det.observe("message_grep", 5), Some(5));
    }

    #[test]
    fn detector_throttles_repeat_warnings_for_same_pair() {
        let mut det = RecurseDepthDetector::new(5);
        assert_eq!(det.observe("file", 5), Some(5));
        // Subsequent observations of the same (scope, depth)
        // stay silent.
        for _ in 0..10 {
            assert_eq!(det.observe("file", 5), None);
        }
    }

    #[test]
    fn detector_fires_separately_for_different_scope_kinds() {
        let mut det = RecurseDepthDetector::new(3);
        assert_eq!(det.observe("file", 3), Some(3));
        // Same depth, different scope — fresh pair.
        assert_eq!(det.observe("message_grep", 3), Some(3));
        assert_eq!(det.observe("tool_result", 3), Some(3));
    }

    #[test]
    fn detector_fires_separately_for_different_depths() {
        let mut det = RecurseDepthDetector::new(3);
        assert_eq!(det.observe("file", 3), Some(3));
        // Same scope, deeper depth — fresh pair.
        assert_eq!(det.observe("file", 4), Some(4));
        assert_eq!(det.observe("file", 5), Some(5));
    }

    #[test]
    fn detector_does_not_abort_session() {
        // Mirrors the failure_loop "no abort sentinel"
        // contract.
        let mut det = RecurseDepthDetector::new(5);
        det.observe("file", 5);
        for _ in 0..50 {
            // Continued observations stay None (throttle), but
            // there's no "stop" signal returned.
            assert_eq!(det.observe("file", 5), None);
        }
    }
}
