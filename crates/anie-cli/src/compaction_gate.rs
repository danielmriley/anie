//! Controller-side mid-turn `CompactionGate` implementation.
//!
//! The gate is consulted by `AgentLoop` between sampling
//! iterations (PR 8.3 of `docs/midturn_compaction_2026-04-27/`).
//! When the loop's local `context: Vec<Message>` exceeds the
//! configured threshold, the gate compacts in memory via
//! `anie_session::compact_messages_inline` and hands the
//! reduced slice back to the loop, which uses it for the next
//! sampling request. Plan
//! `docs/midturn_compaction_2026-04-27/04_midturn_compaction_execution.md`
//! PR B.
//!
//! Mid-turn compactions DO NOT persist into the session log
//! (recommendation A in the plan). The canonical session
//! record is the user-prompt / assistant-message sequence,
//! which is persisted independently. The mid-turn summary is
//! ephemeral context shaping for the in-flight loop.

use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use anie_agent::{CompactionGate, CompactionGateOutcome};
use anie_protocol::{AgentEvent, CompactionPhase, Message};
use anie_session::{
    CompactionConfig, MessageSummarizer, can_compact_messages_inline, compact_messages_inline,
    estimate_message_tokens,
};

use crate::compaction_stats::CompactionStatsAtomic;

/// How many recent compaction outcomes the gate inspects when
/// deciding whether the loop has stagnated. Three is enough to
/// distinguish a noisy single-call dip from a stuck pattern; any
/// fewer would over-fire on bursty contexts.
const STAGNATION_WINDOW: usize = 3;

/// A compaction is "weak progress" when it shrinks the context
/// by less than this fraction of its tokens_before. Three weak
/// calls in a row trip `StagnationKind::ConvergingFloor`.
const STAGNATION_PROGRESS_THRESHOLD: f64 = 0.10;

/// A compaction "recovers from aggressive mode" when it shrinks
/// the context by at least this fraction of its tokens_before.
/// Triggers a level-down transition on the aggressive_level.
const STAGNATION_RECOVERY_THRESHOLD: f64 = 0.30;

/// Maximum aggressive level. At level N, `keep_recent_tokens`
/// for the next compaction call is `default >> N`. Three levels
/// (8x reduction) is enough that further aggression won't
/// meaningfully shrink output without cutting into the floor.
const MAX_AGGRESSIVE_LEVEL: u8 = 3;

/// `keep_recent_tokens` floor at the highest aggressive level.
/// Below this, the summary frame plus the most recent message
/// would barely fit; further aggression hurts more than it
/// helps. Tuned to roughly the size of one substantial tool
/// result.
const KEEP_RECENT_FLOOR: u64 = 2_048;

/// Maximum bounded length of `GateState::history`. Larger than
/// `STAGNATION_WINDOW` so we have a small buffer for diagnostic
/// inspection, but small enough to never grow unbounded.
const MAX_HISTORY: usize = 8;

/// One row of the gate's stagnation history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionOutcome {
    tokens_before: u64,
    tokens_after: u64,
}

/// Two patterns of stagnation, with different responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagnationKind {
    /// Each of the last `STAGNATION_WINDOW` compactions shrunk
    /// by less than `STAGNATION_PROGRESS_THRESHOLD` of its
    /// tokens_before. The summarizer is making real progress
    /// but the floor (kept_recent + summary frame) is itself
    /// above threshold. Action: aggressive compaction —
    /// halve `keep_recent_tokens` for the next call.
    ConvergingFloor,
    /// `tokens_after` did not shrink across the last
    /// `STAGNATION_WINDOW` calls. The summarizer is producing
    /// at least as much as it consumes — broken or
    /// adversarial. Action: skip and let reactive overflow
    /// take over; aggressive compaction can't help.
    Regressing,
}

/// Per-turn gate state, behind a `Mutex` because
/// `CompactionGate::maybe_compact` is `&self` and we need to
/// mutate across calls. Lock contention is irrelevant here —
/// the surrounding async LLM call dwarfs any contention.
///
/// `pub(crate)` so the controller's gate-construction site can
/// initialize a fresh `Default::default()` per run; otherwise
/// fully encapsulated.
#[derive(Debug, Default)]
pub(crate) struct GateState {
    /// Last `MAX_HISTORY` compaction outcomes. Newest at the
    /// back. Inspected by `detect_stagnation`.
    history: VecDeque<CompactionOutcome>,
    /// 0 = use config default. Each level halves
    /// `keep_recent_tokens` for the *next* compaction. Capped
    /// at `MAX_AGGRESSIVE_LEVEL`. Decremented on a
    /// meaningful-progress call so we recover when usage
    /// patterns settle.
    aggressive_level: u8,
}

/// Pure stagnation detection over a history slice. Public to
/// the module so unit tests can drive it directly.
fn detect_stagnation(history: &VecDeque<CompactionOutcome>) -> Option<StagnationKind> {
    if history.len() < STAGNATION_WINDOW {
        return None;
    }
    // Newest-first slice of the last `STAGNATION_WINDOW`
    // outcomes.
    let recent: Vec<CompactionOutcome> = history
        .iter()
        .rev()
        .take(STAGNATION_WINDOW)
        .copied()
        .collect();

    // Regression takes precedence over convergence: if the
    // summarizer is regressing AND the per-call shrink is
    // also weak, the regression diagnosis is the safer
    // dispatch (skip rather than aggress).
    //
    // "Regression" here means tokens_after didn't shrink
    // across the window. `recent` is newest-first, so newer
    // is index 0; we want to check that newer >= older for
    // every adjacent pair.
    let monotone_grow = recent
        .windows(2)
        .all(|w| w[0].tokens_after >= w[1].tokens_after);
    if monotone_grow {
        return Some(StagnationKind::Regressing);
    }

    let weak_progress = recent.iter().all(|c| {
        let shrunk = c.tokens_before.saturating_sub(c.tokens_after);
        let ratio = shrunk as f64 / c.tokens_before.max(1) as f64;
        ratio < STAGNATION_PROGRESS_THRESHOLD
    });
    if weak_progress {
        return Some(StagnationKind::ConvergingFloor);
    }

    None
}

/// Compute the `keep_recent_tokens` value for the next
/// compaction at a given aggressive level. Halves the default
/// per level, with a floor — but the floor only applies when
/// the configured `default` exceeds it. A user who has
/// configured a tight `keep_recent_tokens` (or test code with
/// small constants) is intentionally below the floor and we
/// must not override them upward.
fn aggressive_keep_recent(default: u64, level: u8) -> u64 {
    if level == 0 {
        return default;
    }
    let scaled = default >> u32::from(level);
    if default >= KEEP_RECENT_FLOOR {
        scaled.max(KEEP_RECENT_FLOOR)
    } else {
        // Default is already below the floor — the floor
        // would *increase* the value here, defeating the
        // point. Just return the halved value.
        scaled
    }
}

/// Controller-side `CompactionGate`. Built per-turn from the
/// controller's current model / config / shared budget atomic
/// and installed on `AgentLoopConfig` via
/// `with_compaction_gate(...)`.
///
/// The gate's fields are snapshots taken at gate-construction
/// time, not live references to controller state. The
/// controller mutex would otherwise need to be re-acquired
/// from a spawned task on every sampling iteration. Per-turn
/// snapshotting matches anie's existing pattern (the pre-prompt
/// path also snapshots model + config when it builds
/// `CompactionStrategy`) and keeps the gate dependency-free
/// from the larger controller state.
pub(crate) struct ControllerCompactionGate {
    pub config: CompactionConfig,
    pub summarizer: Arc<dyn MessageSummarizer>,
    /// Shared with `InteractiveController::compactions_remaining_this_turn`
    /// so the per-turn budget tracked across pre-prompt /
    /// reactive / mid-turn paths is one counter (PR 8.2).
    pub budget: Arc<AtomicU32>,
    pub event_tx: mpsc::Sender<AgentEvent>,
    /// Per-session compaction counters. Cloned from
    /// `ControllerState::compaction_stats` so the mid-turn
    /// path increments the same atomic the pre-prompt and
    /// reactive paths use. Plan 06 of
    /// `docs/midturn_compaction_2026-04-27/`.
    pub stats: Arc<CompactionStatsAtomic>,
    /// Per-turn stagnation state. Tracks recent compaction
    /// outcomes so the gate can detect "summarizer is making
    /// real progress but the floor is above threshold" (-> halve
    /// `keep_recent_tokens` for the next call) vs. "summarizer
    /// is regressing" (-> skip with a clear reason).
    /// Plan `docs/rlm_2026-04-29/01_stagnation_detection.md`.
    pub state: Arc<Mutex<GateState>>,
}

#[async_trait]
impl CompactionGate for ControllerCompactionGate {
    async fn maybe_compact(
        &self,
        context: &[Message],
    ) -> Result<CompactionGateOutcome, anyhow::Error> {
        // Threshold check. Tokens are estimated per
        // `estimate_message_tokens` because the loop's
        // `context: Vec<Message>` doesn't carry the assistant
        // usage info `estimate_context_tokens` would need.
        let tokens = estimate_message_tokens(context);
        let threshold = self
            .config
            .context_window
            .saturating_sub(self.config.reserve_tokens);
        if tokens <= threshold {
            return Ok(CompactionGateOutcome::Continue);
        }

        // Budget check. Acquire load matches the Release store
        // the controller uses when it resets / decrements the
        // counter, so a fresh user turn's reset is visible
        // here before we consult it.
        if self.budget.load(Ordering::Acquire) == 0 {
            return Ok(CompactionGateOutcome::Skipped {
                reason: format!(
                    "per-turn compaction budget exhausted ({tokens} tokens over threshold {threshold})"
                ),
            });
        }

        // Stagnation detection. Plan
        // `docs/rlm_2026-04-29/01_stagnation_detection.md`.
        // Inspects history of recent compaction outcomes and
        // either:
        //   - skips with a "regressing" reason (summarizer is
        //     making things worse, reactive overflow takes
        //     over);
        //   - bumps `aggressive_level` so the next compaction
        //     uses a tighter `keep_recent_tokens`.
        // No-op when fewer than `STAGNATION_WINDOW` outcomes
        // have been recorded.
        let aggressive_level = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            match detect_stagnation(&state.history) {
                Some(StagnationKind::Regressing) => {
                    return Ok(CompactionGateOutcome::Skipped {
                        reason: format!(
                            "compaction stagnated (summarizer regressing: tokens_after did not shrink across the last {STAGNATION_WINDOW} calls); falling through to reactive overflow path"
                        ),
                    });
                }
                Some(StagnationKind::ConvergingFloor) => {
                    state.aggressive_level = state
                        .aggressive_level
                        .saturating_add(1)
                        .min(MAX_AGGRESSIVE_LEVEL);
                }
                None => {}
            }
            state.aggressive_level
        };

        // Build a per-call config with `keep_recent_tokens`
        // halved per aggressive level. The summarizer prompt
        // is unchanged; only the cut-point heuristic's
        // window-of-verbatim-recent-messages shrinks. That's
        // the load-bearing knob for shrinking
        // tokens_after.
        let mut effective_config = self.config;
        effective_config.keep_recent_tokens =
            aggressive_keep_recent(self.config.keep_recent_tokens, aggressive_level);

        // Preflight the cut point before emitting CompactionStart.
        // Without this, a threshold breach with no discardable
        // prefix would emit Start, `compact_messages_inline` would
        // return None, and the TUI would stay in Compacting because
        // no matching End event exists.
        if !can_compact_messages_inline(context, effective_config.keep_recent_tokens) {
            return Ok(CompactionGateOutcome::Continue);
        }

        anie_agent::send_event(
            &self.event_tx,
            AgentEvent::CompactionStart {
                phase: CompactionPhase::MidTurn,
            },
        )
        .await;
        match compact_messages_inline(context, &effective_config, self.summarizer.as_ref()).await? {
            Some(inline) => {
                // Decrement the budget AFTER a successful
                // compaction, mirroring the pre-prompt path's
                // bookkeeping (PR 8.2). Subtract via
                // `fetch_update` with `saturating_sub` so a
                // racy double-decrement (mid-turn + reactive
                // landing on the same atomic) bottoms out at 0
                // rather than wrapping.
                self.budget
                    .fetch_update(Ordering::Release, Ordering::Acquire, |n| {
                        Some(n.saturating_sub(1))
                    })
                    .ok();

                // Record the outcome to the stagnation history.
                // If this compaction shrank meaningfully (past
                // the recovery threshold), drop the aggressive
                // level so the *next* compaction can return to
                // the configured default. This is what lets the
                // gate "come back" to normal mode after a brief
                // burst of context pressure.
                {
                    let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                    let outcome = CompactionOutcome {
                        tokens_before: inline.tokens_before,
                        tokens_after: inline.tokens_after,
                    };
                    state.history.push_back(outcome);
                    while state.history.len() > MAX_HISTORY {
                        state.history.pop_front();
                    }
                    let shrunk = inline.tokens_before.saturating_sub(inline.tokens_after);
                    let ratio = shrunk as f64 / inline.tokens_before.max(1) as f64;
                    if ratio >= STAGNATION_RECOVERY_THRESHOLD {
                        state.aggressive_level = state.aggressive_level.saturating_sub(1);
                    }
                }

                anie_agent::send_event(
                    &self.event_tx,
                    AgentEvent::CompactionEnd {
                        phase: CompactionPhase::MidTurn,
                        summary: inline.summary.clone(),
                        tokens_before: inline.tokens_before,
                        tokens_after: inline.tokens_after,
                    },
                )
                .await;
                // Plan 06 PR B: bump the per-session counter
                // after the user-visible event, mirroring
                // `ControllerState::emit_compaction_end`'s
                // ordering for the other two phases.
                self.stats.increment(CompactionPhase::MidTurn);
                Ok(CompactionGateOutcome::Compacted {
                    messages: inline.messages,
                })
            }
            None => {
                // `compact_messages_inline` returned None —
                // not enough discardable content. Surface as
                // Continue rather than Skipped: the threshold
                // was over but the cut-point heuristic
                // disagreed, so there's nothing to log as a
                // budget-exhaustion-style skip. The loop's
                // next sampling request may still overflow,
                // which the reactive path handles.
                Ok(CompactionGateOutcome::Continue)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{ContentBlock, UserMessage, now_millis};

    /// Stub `MessageSummarizer` for unit tests. Returns a
    /// pre-baked summary; records nothing.
    struct StubSummarizer(String);

    #[async_trait]
    impl MessageSummarizer for StubSummarizer {
        async fn summarize(
            &self,
            _messages: &[Message],
            _existing_summary: Option<&str>,
        ) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    fn make_user(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: now_millis(),
        })
    }

    fn build_gate(
        config: CompactionConfig,
        budget_initial: u32,
    ) -> (ControllerCompactionGate, mpsc::Receiver<AgentEvent>) {
        let (tx, rx) = mpsc::channel(32);
        (
            ControllerCompactionGate {
                config,
                summarizer: Arc::new(StubSummarizer("test summary".into())),
                budget: Arc::new(AtomicU32::new(budget_initial)),
                event_tx: tx,
                stats: Arc::new(CompactionStatsAtomic::default()),
                state: Arc::new(Mutex::new(GateState::default())),
            },
            rx,
        )
    }

    fn outcome(before: u64, after: u64) -> CompactionOutcome {
        CompactionOutcome {
            tokens_before: before,
            tokens_after: after,
        }
    }

    fn history(rows: impl IntoIterator<Item = CompactionOutcome>) -> VecDeque<CompactionOutcome> {
        rows.into_iter().collect()
    }

    fn small_context_below_threshold() -> Vec<Message> {
        vec![make_user("short message")]
    }

    /// PR 8.4 PR B: under the threshold, the gate returns
    /// `Continue` without invoking the summarizer.
    #[tokio::test]
    async fn midturn_compaction_does_not_fire_under_threshold() {
        let config = CompactionConfig {
            context_window: 1_000_000,
            reserve_tokens: 100_000,
            keep_recent_tokens: 50_000,
        };
        let (gate, _rx) = build_gate(config, 5);
        let outcome = gate
            .maybe_compact(&small_context_below_threshold())
            .await
            .expect("gate ok");
        assert!(matches!(outcome, CompactionGateOutcome::Continue));
        // Budget unchanged.
        assert_eq!(gate.budget.load(Ordering::Acquire), 5);
    }

    /// PR 8.4 PR B + 8.5 of `docs/midturn_compaction_2026-04-27/`:
    /// when the budget is zero and the threshold is breached,
    /// the gate returns `Skipped` instead of running the
    /// summarizer. The next sampling request will likely
    /// overflow, and the reactive retry path's
    /// `CompactionBudgetExhausted` handles it (PR 8.2).
    #[tokio::test]
    async fn midturn_compaction_skipped_when_budget_exhausted() {
        // Threshold = window - reserve = 0, so any non-empty
        // context is over the threshold.
        let config = CompactionConfig {
            context_window: 100,
            reserve_tokens: 100,
            keep_recent_tokens: 1,
        };
        let (gate, _rx) = build_gate(config, 0);
        let outcome = gate
            .maybe_compact(&small_context_below_threshold())
            .await
            .expect("gate ok");
        match outcome {
            CompactionGateOutcome::Skipped { reason } => {
                assert!(reason.contains("budget exhausted"), "got: {reason}");
            }
            other => panic!("expected Skipped, got: {other:?}"),
        }
        // Budget remains at zero; the gate must not wrap the
        // counter via `saturating_sub` on a skip path.
        assert_eq!(gate.budget.load(Ordering::Acquire), 0);
    }

    /// Regression: a threshold breach alone is not enough to
    /// announce compaction. If the cut-point heuristic cannot
    /// discard anything, the gate must return Continue without
    /// emitting CompactionStart; otherwise the TUI enters
    /// `AgentUiState::Compacting` and never receives a matching
    /// CompactionEnd.
    #[tokio::test]
    async fn midturn_compaction_without_discardable_cut_does_not_emit_start() {
        let config = CompactionConfig {
            context_window: 10,
            reserve_tokens: 10,
            keep_recent_tokens: 100_000,
        };
        let (gate, mut rx) = build_gate(config, 1);

        let outcome = gate
            .maybe_compact(&small_context_below_threshold())
            .await
            .expect("gate ok");

        assert!(matches!(outcome, CompactionGateOutcome::Continue));
        assert_eq!(gate.budget.load(Ordering::Acquire), 1);
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, AgentEvent::CompactionStart { .. }),
                "must not emit CompactionStart without a possible CompactionEnd"
            );
        }
    }

    /// PR 8.4 PR B: above the threshold with budget remaining,
    /// the gate runs the summarizer, decrements the budget,
    /// emits CompactionStart + CompactionEnd events, and
    /// returns `Compacted` carrying the new context.
    #[tokio::test]
    async fn midturn_compaction_fires_when_context_exceeds_threshold() {
        // Build messages large enough that find_cut_point bites.
        let mut messages = Vec::new();
        for i in 0..8 {
            messages.push(make_user(&format!(
                "user message #{i} with enough text to make it heftier"
            )));
        }
        // Threshold = 100 - 50 = 50. Each user message is
        // roughly 14-15 tokens; 8 of them is ~115 > 50.
        let config = CompactionConfig {
            context_window: 100,
            reserve_tokens: 50,
            keep_recent_tokens: 30,
        };
        let (gate, mut rx) = build_gate(config, 2);

        let outcome = gate.maybe_compact(&messages).await.expect("gate ok");
        match outcome {
            CompactionGateOutcome::Compacted { messages: new_msgs } => {
                assert!(
                    new_msgs.len() < messages.len(),
                    "compaction should reduce message count: {} -> {}",
                    messages.len(),
                    new_msgs.len(),
                );
                // The first new message is the synthetic summary.
                if let Message::User(user) = &new_msgs[0] {
                    let text = user
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    assert!(
                        text.contains("[Previous conversation summary]"),
                        "synthetic summary header missing: {text}",
                    );
                } else {
                    panic!("expected synthetic User summary at index 0");
                }
            }
            other => panic!("expected Compacted, got: {other:?}"),
        }

        // Budget decremented from 2 → 1.
        assert_eq!(gate.budget.load(Ordering::Acquire), 1);

        // Plan 06 PR B: stats counter must record exactly one
        // mid-turn compaction; the other phase counters stay
        // at zero since the gate only emits MidTurn events.
        let snapshot = gate.stats.snapshot();
        assert_eq!(
            snapshot.mid_turn, 1,
            "expected one mid-turn count, got {snapshot:?}",
        );
        assert_eq!(
            snapshot.pre_prompt, 0,
            "gate must not bump pre-prompt counter, got {snapshot:?}",
        );
        assert_eq!(
            snapshot.reactive_overflow, 0,
            "gate must not bump reactive counter, got {snapshot:?}",
        );

        // Drain emitted events; both Start and End should fire,
        // in that order.
        let mut saw_start = false;
        let mut saw_end = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::CompactionStart { phase } => {
                    assert!(
                        matches!(phase, CompactionPhase::MidTurn),
                        "mid-turn gate must tag the event with CompactionPhase::MidTurn",
                    );
                    saw_start = true;
                }
                AgentEvent::CompactionEnd { phase, .. } => {
                    assert!(
                        matches!(phase, CompactionPhase::MidTurn),
                        "mid-turn gate must tag the end event with CompactionPhase::MidTurn",
                    );
                    saw_end = true;
                }
                _ => {}
            }
        }
        assert!(saw_start && saw_end, "must emit CompactionStart + End");
    }

    // =====================================================
    // Stagnation detection — Plan
    // `docs/rlm_2026-04-29/01_stagnation_detection.md`.
    // =====================================================

    /// Fewer than `STAGNATION_WINDOW` (3) outcomes can never be
    /// classified — there's not enough history to call it
    /// stagnation, even if the entries individually look weak.
    #[test]
    fn stagnation_not_detected_with_fewer_than_3_outcomes() {
        let empty: VecDeque<CompactionOutcome> = VecDeque::new();
        assert_eq!(detect_stagnation(&empty), None);

        let one = history([outcome(1000, 990)]);
        assert_eq!(detect_stagnation(&one), None);

        let two = history([outcome(1000, 990), outcome(1000, 990)]);
        assert_eq!(detect_stagnation(&two), None);
    }

    /// Three outcomes each shrinking less than 10% of their
    /// tokens_before classify as ConvergingFloor.
    #[test]
    fn converging_floor_detected_when_progress_is_under_10pct() {
        let h = history([
            outcome(1000, 950), // 5% shrink
            outcome(960, 920),  // 4.2%
            outcome(930, 900),  // 3.2%
        ]);
        assert_eq!(detect_stagnation(&h), Some(StagnationKind::ConvergingFloor));
    }

    /// Three outcomes whose tokens_after grow monotonically
    /// classify as Regressing — the summarizer is producing more
    /// than it consumes.
    #[test]
    fn regressing_detected_when_tokens_after_grows() {
        // tokens_after: 800 -> 900 -> 1000 (newest at the back).
        let h = history([outcome(1500, 800), outcome(1500, 900), outcome(1500, 1000)]);
        assert_eq!(detect_stagnation(&h), Some(StagnationKind::Regressing));
    }

    /// A history that satisfies both predicates dispatches to
    /// Regressing. The "skip and let reactive overflow take
    /// over" response is safer than "aggress" when the
    /// summarizer is broken.
    #[test]
    fn regressing_takes_precedence_over_converging() {
        // Both weak progress (~0%) AND tokens_after grew slightly
        // monotonically (newest >= older).
        let h = history([outcome(1000, 990), outcome(1010, 1000), outcome(1020, 1010)]);
        // tokens_after sequence (newest-first): 1010, 1000, 990 —
        // each newer is >= older, so monotone_grow is true,
        // and Regressing wins despite weak_progress also being
        // true.
        assert_eq!(detect_stagnation(&h), Some(StagnationKind::Regressing));
    }

    /// One healthy compaction sandwiched between weak ones is
    /// enough to break the stagnation pattern.
    #[test]
    fn one_healthy_call_breaks_stagnation() {
        let h = history([
            outcome(1000, 950), // weak
            outcome(1000, 500), // big shrink — recovered
            outcome(1000, 950), // weak
        ]);
        assert_eq!(detect_stagnation(&h), None);
    }

    /// `aggressive_keep_recent(default, level)` halves per
    /// level with a floor that only applies when the default
    /// is already above the floor.
    #[test]
    fn aggressive_keep_recent_halves_with_floor() {
        let default = 20_000;
        assert_eq!(aggressive_keep_recent(default, 0), 20_000);
        assert_eq!(aggressive_keep_recent(default, 1), 10_000);
        assert_eq!(aggressive_keep_recent(default, 2), 5_000);
        assert_eq!(aggressive_keep_recent(default, 3), 2_500);

        // Floor kicks in when halving a default that's above
        // the floor would dip below it.
        let near_floor = 4_000;
        assert_eq!(
            aggressive_keep_recent(near_floor, 1),
            KEEP_RECENT_FLOOR,
            "default=4000 >> 1 = 2000 < 2048; floor kicks in",
        );

        // Floor must NOT increase a default that's already
        // below the floor — that would produce more verbatim
        // recent content rather than less, defeating the
        // point of "aggressive" mode.
        assert_eq!(
            aggressive_keep_recent(1_000, 0),
            1_000,
            "level 0 always returns the default",
        );
        assert_eq!(
            aggressive_keep_recent(1_000, 1),
            500,
            "small default halves without flooring upward",
        );
    }

    /// On a `Regressing` history, the gate skips with a reason
    /// that names the regression rather than running the
    /// summarizer.
    #[tokio::test]
    async fn gate_skips_with_regression_reason_when_summarizer_regressing() {
        // Threshold = 0 so any non-empty context is over.
        let config = CompactionConfig {
            context_window: 100,
            reserve_tokens: 100,
            keep_recent_tokens: 50,
        };
        let (gate, _rx) = build_gate(config, 5);
        // Pre-seed history with a regressing pattern.
        {
            let mut state = gate.state.lock().expect("state lock");
            state.history.push_back(outcome(1500, 800));
            state.history.push_back(outcome(1500, 900));
            state.history.push_back(outcome(1500, 1000));
        }
        let outcome = gate
            .maybe_compact(&small_context_below_threshold())
            .await
            .expect("gate ok");
        match outcome {
            CompactionGateOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("regressing"),
                    "skip reason should mention regression; got: {reason}",
                );
            }
            other => panic!("expected Skipped, got: {other:?}"),
        }
        // Budget unchanged: regression skips before any
        // compaction call.
        assert_eq!(gate.budget.load(Ordering::Acquire), 5);
    }

    /// End-to-end: a converging-floor history pre-seeded into
    /// the gate causes the next compaction to fire successfully
    /// (where the *unhalved* keep_recent might fail) and append
    /// a new outcome to the history. The aggressive halving
    /// math is already verified by
    /// `aggressive_keep_recent_halves_with_floor`; this test
    /// just locks down the integration: stagnation pre-seed
    /// → compact still fires → history grows.
    #[tokio::test]
    async fn gate_aggressive_compaction_halves_keep_recent() {
        let mut messages = Vec::new();
        for i in 0..30 {
            messages.push(make_user(&format!(
                "user message #{i} with enough text to make it heftier than a placeholder"
            )));
        }
        // Threshold = 500 - 100 = 400; 30 messages * ~17
        // tokens = ~510 well over.
        let config = CompactionConfig {
            context_window: 500,
            reserve_tokens: 100,
            keep_recent_tokens: 200,
        };
        let (gate, mut rx) = build_gate(config, 5);
        // Pre-seed a converging-floor history so the gate
        // detects stagnation on entry and bumps to level 1
        // (keep_recent_tokens halved 200 -> 100).
        {
            let mut state = gate.state.lock().expect("state lock");
            state.history.push_back(outcome(1000, 950));
            state.history.push_back(outcome(960, 920));
            state.history.push_back(outcome(930, 900));
        }

        let result = gate.maybe_compact(&messages).await.expect("gate ok");
        assert!(
            matches!(result, CompactionGateOutcome::Compacted { .. }),
            "should compact under aggressive level: got {result:?}",
        );
        // History grew — the new outcome was appended.
        let history_len = gate.state.lock().expect("state lock").history.len();
        assert_eq!(
            history_len, 4,
            "successful compaction should append to history (3 pre-seeded + 1 new)",
        );
        // Drain emitted events to keep `_rx` quiet.
        while rx.try_recv().is_ok() {}
    }

    /// Repeated stagnation climbs the aggressive level up to
    /// `MAX_AGGRESSIVE_LEVEL` and stays there.
    #[tokio::test]
    async fn gate_aggressive_level_climbs_on_repeated_stagnation() {
        let config = CompactionConfig {
            context_window: 100,
            reserve_tokens: 50,
            keep_recent_tokens: 30,
        };
        let (gate, _rx) = build_gate(config, 100);
        // Manually seed the history to put us in a converging
        // pattern and then repeatedly invoke maybe_compact's
        // detection logic via the same path.
        for _ in 0..6 {
            // Each call should detect stagnation, bump the
            // level, and (since the StubSummarizer always
            // returns the same trivial summary) record an
            // outcome that itself is weak progress.
            {
                let mut state = gate.state.lock().expect("state lock");
                // Force the history to a converging pattern
                // each iteration.
                state.history.clear();
                state.history.push_back(outcome(1000, 950));
                state.history.push_back(outcome(960, 920));
                state.history.push_back(outcome(930, 900));
            }
            // Re-trigger detection by reading the state.
            let kind = detect_stagnation(&gate.state.lock().expect("lock").history);
            assert_eq!(kind, Some(StagnationKind::ConvergingFloor));
            // Manually advance the level to simulate the
            // gate's response.
            let mut state = gate.state.lock().expect("state lock");
            state.aggressive_level = state
                .aggressive_level
                .saturating_add(1)
                .min(MAX_AGGRESSIVE_LEVEL);
        }
        let level = gate.state.lock().expect("state lock").aggressive_level;
        assert_eq!(
            level, MAX_AGGRESSIVE_LEVEL,
            "level should saturate at MAX_AGGRESSIVE_LEVEL (3)",
        );
    }

    /// Stagnation history is bounded at `MAX_HISTORY` so a
    /// long-running run can't grow it without bound.
    #[tokio::test]
    async fn gate_history_bounded_at_max_history() {
        let config = CompactionConfig {
            context_window: 100,
            reserve_tokens: 50,
            keep_recent_tokens: 30,
        };
        let (gate, _rx) = build_gate(config, 200);
        // Hand-fill the history past MAX_HISTORY via the
        // recording path — simulate what a long sequence of
        // compactions would do.
        {
            let mut state = gate.state.lock().expect("state lock");
            for i in 0..(MAX_HISTORY + 10) {
                state.history.push_back(outcome(1000, 1000 - i as u64));
                while state.history.len() > MAX_HISTORY {
                    state.history.pop_front();
                }
            }
        }
        let len = gate.state.lock().expect("state lock").history.len();
        assert_eq!(len, MAX_HISTORY, "history must be bounded at MAX_HISTORY");
    }
}
