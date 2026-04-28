//! Per-session compaction counters.
//!
//! Three different code paths trigger compaction:
//!
//! 1. Pre-prompt (`InteractiveController::maybe_auto_compact`).
//! 2. Mid-turn (`ControllerCompactionGate`).
//! 3. Reactive overflow recovery (`retry_after_overflow`).
//!
//! Without per-phase counters, "why did this session feel slow"
//! is unanswerable after the fact. This module owns a small
//! shared atomic struct each path increments after a successful
//! compaction. The `/state` summary surfaces the snapshot so
//! users can inspect it without trawling logs. Plan
//! `docs/midturn_compaction_2026-04-27/06_compaction_telemetry.md`
//! PR B.

use std::sync::atomic::{AtomicU32, Ordering};

use anie_protocol::CompactionPhase;

/// Atomic counters for the three compaction phases. Shared via
/// `Arc` between the controller and the mid-turn gate so all
/// paths agree on the same counts. Acquire/Release ordering
/// matches the per-turn budget atomic next door
/// (`compactions_remaining_this_turn`).
#[derive(Debug, Default)]
pub(crate) struct CompactionStatsAtomic {
    pre_prompt: AtomicU32,
    mid_turn: AtomicU32,
    reactive_overflow: AtomicU32,
}

impl CompactionStatsAtomic {
    /// Bump the counter for the given phase. Called by every
    /// successful compaction emit site.
    pub(crate) fn increment(&self, phase: CompactionPhase) {
        let counter = match phase {
            CompactionPhase::PrePrompt => &self.pre_prompt,
            CompactionPhase::MidTurn => &self.mid_turn,
            CompactionPhase::ReactiveOverflow => &self.reactive_overflow,
        };
        counter.fetch_add(1, Ordering::Release);
    }

    /// Plain-old-data snapshot for rendering / serialization.
    pub(crate) fn snapshot(&self) -> CompactionStats {
        CompactionStats {
            pre_prompt: self.pre_prompt.load(Ordering::Acquire),
            mid_turn: self.mid_turn.load(Ordering::Acquire),
            reactive_overflow: self.reactive_overflow.load(Ordering::Acquire),
        }
    }

    /// Reset all counters to zero. Called when the user starts
    /// a fresh session (`/new`) or switches sessions, since the
    /// counters are session-scoped, not process-scoped.
    pub(crate) fn reset(&self) {
        self.pre_prompt.store(0, Ordering::Release);
        self.mid_turn.store(0, Ordering::Release);
        self.reactive_overflow.store(0, Ordering::Release);
    }
}

/// Plain-data snapshot of [`CompactionStatsAtomic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct CompactionStats {
    pub pre_prompt: u32,
    pub mid_turn: u32,
    pub reactive_overflow: u32,
}

impl CompactionStats {
    pub(crate) fn total(&self) -> u32 {
        self.pre_prompt
            .saturating_add(self.mid_turn)
            .saturating_add(self.reactive_overflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn increment_routes_to_correct_phase_counter() {
        let stats = CompactionStatsAtomic::default();
        stats.increment(CompactionPhase::PrePrompt);
        stats.increment(CompactionPhase::PrePrompt);
        stats.increment(CompactionPhase::MidTurn);
        stats.increment(CompactionPhase::ReactiveOverflow);
        let snap = stats.snapshot();
        assert_eq!(
            snap,
            CompactionStats {
                pre_prompt: 2,
                mid_turn: 1,
                reactive_overflow: 1,
            },
        );
        assert_eq!(snap.total(), 4);
    }

    #[test]
    fn reset_zeroes_all_counters() {
        let stats = CompactionStatsAtomic::default();
        stats.increment(CompactionPhase::PrePrompt);
        stats.increment(CompactionPhase::MidTurn);
        stats.reset();
        assert_eq!(stats.snapshot(), CompactionStats::default());
    }
}
