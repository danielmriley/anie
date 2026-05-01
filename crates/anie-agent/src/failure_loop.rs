//! Failure-loop detector. PR 2 of
//! `docs/harness_mitigations_2026-05-01/`.
//!
//! Tracks consecutive tool failures with the same
//! `(tool_name, args_hash)` pair. When the strike count
//! crosses a configurable threshold, [`FailureLoopDetector::observe`]
//! returns the new strike count exactly once per pair per
//! detector lifetime — the controller turns that signal into a
//! `tracing::info!` log + a `SystemMessage` event surfaced in
//! the transcript.
//!
//! The detector is observability-only: it never aborts the
//! run. Per series principle in `README.md`, hard caps are
//! deferred until the smoke shows observability is
//! insufficient.
//!
//! Thread safety: the detector is intended to be wrapped in a
//! `Mutex` by callers. Concurrent `observe` calls (parallel
//! tool execution) may interleave such that strikes count one
//! ahead of strict sequential ordering — documented as
//! acceptable since the goal is "warn loud enough", not
//! exact-strike accounting.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// Default number of consecutive same-args failures before a
/// warning fires. The controller can override via
/// `ANIE_FAILURE_LOOP_WARN_AT`.
pub const DEFAULT_FAILURE_LOOP_THRESHOLD: u32 = 3;

/// Tracks consecutive tool failures for the failure-loop
/// detector. See module-level docs.
#[derive(Debug)]
pub(crate) struct FailureLoopDetector {
    threshold: u32,
    /// Last `(tool_name, args_hash, strike_count)` triple
    /// observed. `None` after a successful call or before any
    /// observation.
    last: Option<(String, u64, u32)>,
    /// `(tool_name, args_hash)` pairs we've already warned
    /// about — throttle: warn once per pair per detector
    /// lifetime even if the loop keeps going.
    warned: HashSet<(String, u64)>,
}

impl FailureLoopDetector {
    /// Build a new detector. `threshold` is the number of
    /// consecutive failures (with the same tool + args) needed
    /// to fire a warning. A threshold of `0` disables warning
    /// emission while still tracking; the controller would
    /// generally use [`Self::disabled`] instead.
    pub(crate) fn new(threshold: u32) -> Self {
        Self {
            threshold,
            last: None,
            warned: HashSet::new(),
        }
    }

    /// Record a tool result. Returns `Some(strike_count)` when
    /// the threshold is just crossed for a new
    /// `(tool_name, args_hash)` pair, `None` otherwise. A
    /// successful call resets the streak. Different
    /// `(tool_name, args_hash)` resets the streak.
    pub(crate) fn observe(
        &mut self,
        tool_name: &str,
        args: &Value,
        is_error: bool,
    ) -> Option<u32> {
        if !is_error {
            self.last = None;
            return None;
        }

        let hash = stable_args_hash(args);
        let new_count = match &self.last {
            Some((name, h, count)) if name == tool_name && *h == hash => count + 1,
            _ => 1,
        };
        self.last = Some((tool_name.to_string(), hash, new_count));

        if new_count < self.threshold {
            return None;
        }

        let key = (tool_name.to_string(), hash);
        if self.warned.contains(&key) {
            return None;
        }
        self.warned.insert(key);
        Some(new_count)
    }
}

/// Stable hash of a JSON `Value`. Object keys are
/// alphabetized (via [`canonicalize`]) before serialization so
/// `{"a":1,"b":2}` and `{"b":2,"a":1}` collide. Numeric
/// equivalence (e.g., `1` vs `1.0`) is *not* normalized — the
/// underlying JSON serialization preserves the original
/// representation. That's fine for our purpose: the model
/// produces deterministic argument shapes per call site.
fn stable_args_hash(value: &Value) -> u64 {
    let canonical = canonicalize(value);
    let serialized = serde_json::to_string(&canonical).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for key in keys {
                if let Some(v) = map.get(key) {
                    out.insert(key.clone(), canonicalize(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn loop_detector_strikes_only_when_args_hash_matches() {
        let mut det = FailureLoopDetector::new(3);
        // Same tool, different args — never crosses threshold.
        for i in 0..10 {
            let result = det.observe("bash", &json!({ "command": format!("ls {i}") }), true);
            assert_eq!(result, None, "iteration {i} should not warn");
        }
    }

    #[test]
    fn loop_detector_resets_on_success() {
        let mut det = FailureLoopDetector::new(3);
        let args = json!({ "command": "ls" });
        det.observe("bash", &args, true);
        det.observe("bash", &args, true);
        // success in the middle resets — next failure starts fresh
        det.observe("bash", &args, false);
        assert_eq!(det.observe("bash", &args, true), None);
        assert_eq!(det.observe("bash", &args, true), None);
        // third strike now fires
        assert_eq!(det.observe("bash", &args, true), Some(3));
    }

    #[test]
    fn loop_detector_emits_warning_at_configured_threshold() {
        let mut det = FailureLoopDetector::new(3);
        let args = json!({ "command": "ls" });
        assert_eq!(det.observe("bash", &args, true), None);
        assert_eq!(det.observe("bash", &args, true), None);
        assert_eq!(det.observe("bash", &args, true), Some(3));

        let mut det5 = FailureLoopDetector::new(5);
        for _ in 0..4 {
            assert_eq!(det5.observe("bash", &args, true), None);
        }
        assert_eq!(det5.observe("bash", &args, true), Some(5));
    }

    #[test]
    fn loop_detector_does_not_abort_session() {
        // Detector returns None after the first warning — never
        // signals "abort". Demonstrates the observability-only
        // contract.
        let mut det = FailureLoopDetector::new(3);
        let args = json!({ "command": "ls" });
        det.observe("bash", &args, true);
        det.observe("bash", &args, true);
        let first = det.observe("bash", &args, true);
        assert_eq!(first, Some(3));

        // Continue observing the same loop. Threshold already
        // crossed; subsequent observations stay None (throttle)
        // — but the detector does NOT return a "stop" sentinel.
        for _ in 0..50 {
            assert_eq!(det.observe("bash", &args, true), None);
        }
    }

    #[test]
    fn loop_detector_args_hash_stable_across_field_order() {
        let mut det = FailureLoopDetector::new(3);
        let args_ab = json!({ "a": 1, "b": 2 });
        let args_ba = json!({ "b": 2, "a": 1 });
        det.observe("bash", &args_ab, true);
        det.observe("bash", &args_ba, true);
        // If hashes differed, this would be strike 1 of a new
        // streak and thus None. Asserting Some(3) confirms the
        // canonicalization treated them as identical.
        assert_eq!(det.observe("bash", &args_ab, true), Some(3));
    }

    #[test]
    fn loop_detector_throttles_repeat_warnings_for_same_pair() {
        // Series principle: warn once per pair per session.
        let mut det = FailureLoopDetector::new(2);
        let args = json!({ "command": "ls" });
        assert_eq!(det.observe("bash", &args, true), None);
        assert_eq!(det.observe("bash", &args, true), Some(2));
        // Subsequent strikes for the same pair stay silent.
        for _ in 0..5 {
            assert_eq!(det.observe("bash", &args, true), None);
        }
    }

    #[test]
    fn loop_detector_different_tool_name_resets_streak() {
        let mut det = FailureLoopDetector::new(3);
        let args = json!({ "command": "ls" });
        det.observe("bash", &args, true);
        det.observe("bash", &args, true);
        // Different tool with the same args — fresh streak.
        assert_eq!(det.observe("edit", &args, true), None);
        // bash streak no longer adjacent; back to 1.
        assert_eq!(det.observe("bash", &args, true), None);
    }

    #[test]
    fn canonicalize_recurses_into_nested_objects() {
        let nested_a = json!({ "outer": { "z": 1, "a": 2 } });
        let nested_b = json!({ "outer": { "a": 2, "z": 1 } });
        assert_eq!(stable_args_hash(&nested_a), stable_args_hash(&nested_b));
    }
}
