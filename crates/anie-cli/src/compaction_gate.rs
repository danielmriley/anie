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

use std::sync::{
    Arc,
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

        // Preflight the cut point before emitting CompactionStart.
        // Without this, a threshold breach with no discardable
        // prefix would emit Start, `compact_messages_inline` would
        // return None, and the TUI would stay in Compacting because
        // no matching End event exists.
        if !can_compact_messages_inline(context, self.config.keep_recent_tokens) {
            return Ok(CompactionGateOutcome::Continue);
        }

        anie_agent::send_event(
            &self.event_tx,
            AgentEvent::CompactionStart {
                phase: CompactionPhase::MidTurn,
            },
        )
        .await;
        match compact_messages_inline(context, &self.config, self.summarizer.as_ref()).await? {
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
            },
            rx,
        )
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
}
