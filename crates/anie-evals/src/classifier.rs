//! Edit-expectation classifier accuracy scoring.
//!
//! The edit-completion guard (`docs/edit_completion_guard/`) gates on
//! a model-judged "does this task require editing files?" classifier
//! when no explicit `--require-edit` override is set. That classifier
//! is a known weak point for small models, so the guard plan
//! (§"Gating") commits to *measuring* its accuracy: SWE-bench-style
//! "fix the issue" tasks are ground-truth edit-expected, so a verdict
//! of "no" on one is a **false negative** — the exact failure mode
//! that would let an empty-patch run terminate unguarded.
//!
//! The verdict reaches the eval harness as
//! [`EditGuardView::classified_expected`](crate::EditGuardView), an
//! `Option<bool>` folded off `AgentEvent::EditGuard` in the metrics
//! sidecar:
//!
//! - `Some(true)` / `Some(false)`: the classifier ran and decided.
//! - `None`: an explicit override short-circuited the classifier (the
//!   benchmark sets `--require-edit`), so no classification was
//!   performed — correct by construction, never a measured miss.
//!
//! This module is the pure scoring kernel; it has no I/O and is fully
//! deterministic, so the classifier-accuracy eval runs without a live
//! model.

use crate::EditGuardView;

/// One run's classifier outcome, scored against the scenario's
/// ground-truth edit-expectation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifierOutcome {
    /// The classifier ran and agreed with the ground truth.
    Correct,
    /// Ground truth was edit-expected but the classifier said "no" —
    /// the false negative the guard plan measures (an empty-patch run
    /// would terminate unguarded).
    FalseNegative,
    /// Ground truth was NOT edit-expected but the classifier said
    /// "yes" — a false positive (the guard may push a no-edit task to
    /// produce a spurious change; bounded by the suppression
    /// heuristic and round budget).
    FalsePositive,
    /// No classification was performed (an explicit `--require-edit`
    /// override short-circuited the classifier). Not scored as a
    /// hit or miss — there was nothing to get wrong.
    NotClassified,
}

/// Score one run's classifier verdict against the scenario's
/// ground-truth edit-expectation.
///
/// `ground_truth_edit_expected` is the scenario author's known answer
/// — `true` for a SWE-bench-style "fix the issue" task, `false` for a
/// question-style task. The verdict comes from the metrics sidecar.
#[must_use]
pub fn score_classifier(
    guard: &EditGuardView,
    ground_truth_edit_expected: bool,
) -> ClassifierOutcome {
    match guard.classified_expected {
        None => ClassifierOutcome::NotClassified,
        Some(verdict) if verdict == ground_truth_edit_expected => ClassifierOutcome::Correct,
        Some(true) => ClassifierOutcome::FalsePositive,
        Some(false) => ClassifierOutcome::FalseNegative,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(classified: Option<bool>) -> EditGuardView {
        EditGuardView {
            classified_expected: classified,
            ..EditGuardView::default()
        }
    }

    /// The classifier-accuracy eval, deterministically: a
    /// SWE-bench-style "fix the issue" task is ground-truth
    /// edit-expected, so a "yes" verdict scores Correct and the
    /// risky "no" verdict scores a measurable FalseNegative.
    #[test]
    fn fix_the_issue_task_yes_is_correct_no_is_a_false_negative() {
        // Ground truth: a "fix the issue" SWE-bench task DOES require
        // an edit.
        let fix_task_ground_truth = true;

        assert_eq!(
            score_classifier(&guard(Some(true)), fix_task_ground_truth),
            ClassifierOutcome::Correct,
            "a 'yes' verdict on a fix-the-issue task is correct",
        );
        assert_eq!(
            score_classifier(&guard(Some(false)), fix_task_ground_truth),
            ClassifierOutcome::FalseNegative,
            "a 'no' verdict on a fix-the-issue task is the measured miss",
        );
    }

    /// The other ground-truth pole: a question-style task is NOT
    /// edit-expected, so "no" is correct and "yes" is a false
    /// positive. Pinning both poles is what makes the false-negative
    /// rate meaningful (the eval can tell a real miss from the
    /// classifier simply always answering "yes").
    #[test]
    fn question_task_no_is_correct_yes_is_a_false_positive() {
        let question_ground_truth = false;

        assert_eq!(
            score_classifier(&guard(Some(false)), question_ground_truth),
            ClassifierOutcome::Correct,
            "a 'no' verdict on a question task is correct",
        );
        assert_eq!(
            score_classifier(&guard(Some(true)), question_ground_truth),
            ClassifierOutcome::FalsePositive,
            "a 'yes' verdict on a question task is a false positive",
        );
    }

    /// An explicit `--require-edit` override skips the classifier, so
    /// `classified_expected` is absent — scored as NotClassified for
    /// either ground truth (nothing was judged, so nothing can be
    /// wrong).
    #[test]
    fn explicit_override_is_not_scored_as_a_hit_or_miss() {
        for ground_truth in [true, false] {
            assert_eq!(
                score_classifier(&guard(None), ground_truth),
                ClassifierOutcome::NotClassified,
            );
        }
    }
}
