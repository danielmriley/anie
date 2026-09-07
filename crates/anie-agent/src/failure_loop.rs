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

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
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
    /// Per-file text-match failure strikes for `edit`/
    /// `apply_patch`, keyed by the path the tool reported.
    /// Unlike `last`, this survives args changes (a retried
    /// edit with a tweaked `oldText` is a new args hash but
    /// the same stuck file) and is cleared per-path on a
    /// successful mutation. Plan 03 §2c of
    /// `docs/local_model_augmentation/`.
    file_match_failures: HashMap<String, u32>,
}

impl FailureLoopDetector {
    /// Build a new detector. `threshold` is the number of
    /// consecutive failures (with the same tool + args) needed
    /// to fire a warning. To disable the detector entirely the
    /// controller skips construction altogether: the agent-loop
    /// config carries `failure_loop_threshold: Option<u32>` and
    /// only installs a detector when it is `Some(n)`.
    pub(crate) fn new(threshold: u32) -> Self {
        Self {
            threshold,
            last: None,
            warned: HashSet::new(),
            file_match_failures: HashMap::new(),
        }
    }

    /// Record a text-match failure for `path` and return the
    /// new per-file strike count (including this one). The
    /// grounding hook attaches file content only from the
    /// second strike onward — the first failure stays
    /// directive-only.
    pub(crate) fn record_file_match_failure(&mut self, path: &str) -> u32 {
        let strikes = self
            .file_match_failures
            .entry(path.to_string())
            .or_insert(0);
        *strikes += 1;
        *strikes
    }

    /// Clear `path`'s match-failure strikes after a successful
    /// mutation of that file — the model got unstuck, so a
    /// later unrelated match failure starts fresh.
    pub(crate) fn clear_file_match_failures(&mut self, path: &str) {
        self.file_match_failures.remove(path);
    }

    /// Record a tool result. Returns `Some(strike_count)` when
    /// the threshold is just crossed for a new
    /// `(tool_name, args_hash)` pair, `None` otherwise. A
    /// successful call resets the streak. Different
    /// `(tool_name, args_hash)` resets the streak.
    pub(crate) fn observe(&mut self, tool_name: &str, args: &Value, is_error: bool) -> Option<u32> {
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

/// Similarity streak length at which the near-duplicate note
/// fires. Plan 03 §9 (Signal C) of
/// `docs/local_model_augmentation/`.
pub(crate) const SIMILAR_CALL_NOTE_STREAK: u32 = 3;

/// Similarity streak length at which the shared temperature
/// perturbation slot is armed.
pub(crate) const SIMILAR_CALL_PERTURB_STREAK: u32 = 5;

/// Token-set Jaccard overlap above which two same-tool calls
/// count as near-duplicates. Chosen high to spare legitimate
/// paging-through-results sequences; calls that differ only in
/// numeric leaves (read `offset`/`limit` pagination) are exempt
/// outright — see [`SimilarCallDetector::observe`].
const SIMILAR_CALL_JACCARD_THRESHOLD: f64 = 0.6;

/// How many recent calls per tool a new call is compared
/// against.
const SIMILAR_CALL_HISTORY: usize = 5;

/// What the similar-call detector asks the agent loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimilarCallSignal {
    /// Streak hit [`SIMILAR_CALL_NOTE_STREAK`]: emit the
    /// SystemMessage + harness note in the tool result.
    Note { streak: u32 },
    /// Streak hit [`SIMILAR_CALL_PERTURB_STREAK`]: arm the
    /// plan-03 PR10 temperature perturbation slot.
    Perturb { streak: u32 },
}

/// Signal C — near-duplicate call loops. Plan 03 §9 of
/// `docs/local_model_augmentation/`, field notes F4: the
/// small-model loop signature is *similar, successful,
/// useless* calls (ten escalating `web_search` queries, none
/// erroring), which the failure-loop detector — identical
/// `(tool, args_hash)` AND `is_error` — can never catch.
///
/// Tracks the last [`SIMILAR_CALL_HISTORY`] calls per tool as
/// token sets over the string-bearing argument values. A call
/// whose Jaccard overlap with any recent same-tool call
/// exceeds [`SIMILAR_CALL_JACCARD_THRESHOLD`] extends the
/// streak REGARDLESS of is_error; a dissimilar call or a
/// different tool restarts it. The streak counts the cluster
/// size — the first call of a cluster is streak 1, so streak
/// 3 means "the last 3 calls were near-duplicates".
///
/// Pagination exemption (plan 03 §9 FP risk): chunked reads of
/// one file — `{"path": …, "offset": 500}` then `offset: 1000`
/// — tokenize to the path tokens alone, so consecutive chunks
/// are maximally similar by Jaccard. A pair whose arguments
/// differ ONLY in numeric leaves is therefore never counted as
/// similar; exact repeats (identical full args) still are.
#[derive(Debug)]
pub(crate) struct SimilarCallDetector {
    history: HashMap<String, VecDeque<CallShape>>,
    streak: u32,
    streak_tool: Option<String>,
}

/// One recorded call: the similarity token set plus the two
/// canonical hashes the pagination exemption compares.
#[derive(Debug)]
struct CallShape {
    tokens: HashSet<String>,
    args_hash: u64,
    /// [`stable_args_hash`] of the args with numeric leaves
    /// nulled out. Equal stripped hashes with different full
    /// hashes means the pair differs only in numeric knobs.
    numeric_stripped_hash: u64,
}

impl SimilarCallDetector {
    pub(crate) fn new() -> Self {
        Self {
            history: HashMap::new(),
            streak: 0,
            streak_tool: None,
        }
    }

    /// Record a tool call (success or failure — Signal C is
    /// about repetition, not errors). Returns a signal exactly
    /// when the streak reaches the note or perturb threshold.
    pub(crate) fn observe(&mut self, tool_name: &str, args: &Value) -> Option<SimilarCallSignal> {
        let shape = CallShape {
            tokens: argument_tokens(args),
            args_hash: stable_args_hash(args),
            numeric_stripped_hash: stable_args_hash(&strip_numeric_leaves(args)),
        };
        let history = self.history.entry(tool_name.to_string()).or_default();
        let similar = !shape.tokens.is_empty()
            && history.iter().any(|prev| {
                if prev.args_hash == shape.args_hash {
                    // Exact repeat — the strongest loop signal.
                    return true;
                }
                if prev.numeric_stripped_hash == shape.numeric_stripped_hash {
                    // Differs only in numeric leaves: offset/limit
                    // pagination, not a rephrase loop.
                    return false;
                }
                jaccard(&prev.tokens, &shape.tokens) > SIMILAR_CALL_JACCARD_THRESHOLD
            });
        history.push_back(shape);
        if history.len() > SIMILAR_CALL_HISTORY {
            history.pop_front();
        }

        if similar && self.streak_tool.as_deref() == Some(tool_name) {
            self.streak += 1;
        } else {
            // This call starts (or restarts) a potential cluster.
            self.streak = 1;
            self.streak_tool = Some(tool_name.to_string());
        }
        match self.streak {
            SIMILAR_CALL_NOTE_STREAK => Some(SimilarCallSignal::Note {
                streak: self.streak,
            }),
            SIMILAR_CALL_PERTURB_STREAK => Some(SimilarCallSignal::Perturb {
                streak: self.streak,
            }),
            _ => None,
        }
    }
}

/// Token set over every string value in the arguments,
/// recursively. Reimplements the tokenizer convention from
/// `context_virt`'s keyword overlap (lowercase, alphanumeric
/// split, 3-char minimum) — locally, because `anie-agent`
/// must not depend on `anie-cli`. Non-string scalars are
/// ignored: similarity is about the model rephrasing the same
/// query, not about numeric knobs.
fn argument_tokens(args: &Value) -> HashSet<String> {
    let mut tokens = HashSet::new();
    collect_string_tokens(args, &mut tokens);
    tokens
}

fn collect_string_tokens(value: &Value, out: &mut HashSet<String>) {
    match value {
        Value::String(text) => {
            for token in text
                .to_ascii_lowercase()
                .split(|c: char| !c.is_alphanumeric())
            {
                if token.len() >= 3 {
                    out.insert(token.to_string());
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_string_tokens(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_string_tokens(item, out);
            }
        }
        _ => {}
    }
}

/// Replace every numeric leaf with `null`, preserving shape, so
/// [`stable_args_hash`] of the result identifies calls that
/// differ only in numeric knobs (the pagination exemption).
fn strip_numeric_leaves(value: &Value) -> Value {
    match value {
        Value::Number(_) => Value::Null,
        Value::Array(items) => Value::Array(items.iter().map(strip_numeric_leaves).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), strip_numeric_leaves(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let intersection = a.intersection(b).count();
    let union = a.len() + b.len() - intersection;
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

/// Stable hash of a JSON `Value`. Object keys are
/// alphabetized (via [`canonicalize`]) before serialization so
/// `{"a":1,"b":2}` and `{"b":2,"a":1}` collide. Numeric
/// equivalence (e.g., `1` vs `1.0`) is *not* normalized — the
/// underlying JSON serialization preserves the original
/// representation. That's fine for our purpose: the model
/// produces deterministic argument shapes per call site.
///
/// Exposed as `pub` so callers in other crates (e.g.,
/// `anie-cli`'s context-virt eviction policy) can reuse the
/// same hash to identify supersedable failed-vs-successful
/// pairs by `(tool_name, args_hash)`.
pub fn stable_args_hash(value: &Value) -> u64 {
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

    #[test]
    fn file_match_failure_strikes_count_per_path_and_clear_independently() {
        let mut det = FailureLoopDetector::new(3);
        assert_eq!(det.record_file_match_failure("src/a.rs"), 1);
        assert_eq!(det.record_file_match_failure("src/b.rs"), 1);
        assert_eq!(det.record_file_match_failure("src/a.rs"), 2);
        // Clearing one path leaves the other's strikes intact.
        det.clear_file_match_failures("src/a.rs");
        assert_eq!(det.record_file_match_failure("src/a.rs"), 1);
        assert_eq!(det.record_file_match_failure("src/b.rs"), 2);
    }

    // Signal C: the field-notes F4 loop — escalating
    // near-duplicate queries that all "succeed". The third
    // member of the cluster fires the note.
    #[test]
    fn near_duplicate_streak_fires_note_at_three() {
        let mut det = SimilarCallDetector::new();
        assert_eq!(
            det.observe("web_search", &json!({"query": "what time is it right now"})),
            None
        );
        assert_eq!(
            det.observe(
                "web_search",
                &json!({"query": "what time is it right now today"})
            ),
            None
        );
        assert_eq!(
            det.observe(
                "web_search",
                &json!({"query": "current time right now what is it"})
            ),
            Some(SimilarCallSignal::Note { streak: 3 })
        );
    }

    #[test]
    fn dissimilar_call_resets_the_streak() {
        let mut det = SimilarCallDetector::new();
        det.observe("web_search", &json!({"query": "what time is it right now"}));
        det.observe(
            "web_search",
            &json!({"query": "what time is it right now today"}),
        );
        // A genuinely different query restarts the cluster…
        assert_eq!(
            det.observe(
                "web_search",
                &json!({"query": "rust borrow checker lifetimes"})
            ),
            None
        );
        // …so two more near-duplicates of IT are still below
        // the note threshold at streak 2.
        assert_eq!(
            det.observe(
                "web_search",
                &json!({"query": "rust borrow checker lifetimes rules"})
            ),
            None
        );
        assert_eq!(
            det.observe(
                "web_search",
                &json!({"query": "borrow checker lifetimes rust explained"})
            ),
            Some(SimilarCallSignal::Note { streak: 3 })
        );
    }

    #[test]
    fn different_tool_call_resets_the_similarity_streak() {
        let mut det = SimilarCallDetector::new();
        det.observe("web_search", &json!({"query": "what time is it right now"}));
        det.observe(
            "web_search",
            &json!({"query": "what time is it right now today"}),
        );
        // Interleaved different tool breaks the streak even
        // though the per-tool history survives.
        det.observe("read", &json!({"path": "README.md"}));
        assert_eq!(
            det.observe(
                "web_search",
                &json!({"query": "what time is it right now please"})
            ),
            None,
            "streak must restart after a different tool"
        );
    }

    #[test]
    fn streak_of_five_arms_temperature_perturbation() {
        let mut det = SimilarCallDetector::new();
        let args = json!({"query": "what time is it right now"});
        assert_eq!(det.observe("web_search", &args), None);
        assert_eq!(det.observe("web_search", &args), None);
        assert_eq!(
            det.observe("web_search", &args),
            Some(SimilarCallSignal::Note { streak: 3 })
        );
        assert_eq!(det.observe("web_search", &args), None);
        assert_eq!(
            det.observe("web_search", &args),
            Some(SimilarCallSignal::Perturb { streak: 5 })
        );
        // Past the perturb threshold the detector stays quiet.
        assert_eq!(det.observe("web_search", &args), None);
    }

    // Plan 03 §9 FP risk: chunked reads of one file share all
    // their string tokens (the path), so Jaccard alone would
    // call them maximally similar and arm the perturbation on a
    // healthy run. The numeric-leaf exemption must spare the
    // whole sequence — note AND perturb thresholds.
    #[test]
    fn paginated_reads_differing_only_in_offset_never_fire() {
        let mut det = SimilarCallDetector::new();
        for chunk in 0..6 {
            let args = json!({
                "path": "crates/anie-cli/src/controller.rs",
                "offset": chunk * 500,
                "limit": 500,
            });
            assert_eq!(det.observe("read", &args), None, "chunk {chunk}");
        }
    }

    #[test]
    fn exact_repeats_with_numeric_args_still_count_as_similar() {
        // The exemption is for numeric-only DIFFERENCES; an
        // identical re-read of the same chunk is still a loop.
        let mut det = SimilarCallDetector::new();
        let args = json!({"path": "src/lib.rs", "offset": 500, "limit": 500});
        assert_eq!(det.observe("read", &args), None);
        assert_eq!(det.observe("read", &args), None);
        assert_eq!(
            det.observe("read", &args),
            Some(SimilarCallSignal::Note { streak: 3 })
        );
    }

    #[test]
    fn rephrased_queries_with_identical_numeric_knobs_still_fire_the_note() {
        // Numbers present but the string payload changes too —
        // the rephrase loop must still be caught via Jaccard.
        let mut det = SimilarCallDetector::new();
        let query = |q: &str| json!({"query": q, "max_results": 5});
        assert_eq!(
            det.observe("web_search", &query("what time is it right now")),
            None
        );
        assert_eq!(
            det.observe("web_search", &query("what time is it right now today")),
            None
        );
        assert_eq!(
            det.observe("web_search", &query("what time is it right now please")),
            Some(SimilarCallSignal::Note { streak: 3 })
        );
    }

    #[test]
    fn token_free_arguments_never_register_as_similar() {
        // All-numeric args produce an empty token set; identical
        // empty sets must not count as near-duplicates (the
        // failure-loop detector owns the identical-args case).
        let mut det = SimilarCallDetector::new();
        let args = json!({"limit": 5, "offset": 10});
        for _ in 0..6 {
            assert_eq!(det.observe("count", &args), None);
        }
    }

    #[test]
    fn similarity_tokens_are_lowercased_alphanumeric_min_three_chars() {
        let tokens = argument_tokens(&json!({
            "query": "Find the TIME-zone of x in db_v2!"
        }));
        // "of"/"x"/"in"/"db"/"v2" are under the 3-char minimum.
        let expected: HashSet<String> = ["find", "the", "time", "zone"]
            .iter()
            .map(|t| (*t).to_string())
            .collect();
        assert_eq!(tokens, expected);
    }
}
