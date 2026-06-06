//! Session-scoped cost/token accounting.
//!
//! Mirrors the `compaction_stats` pattern: a small meter the controller
//! updates at turn boundaries and reads on `/state` and for the status
//! bar. Token counts are accumulated as `u64`; dollar cost is derived
//! once at read time via the model's `CostPerMillion`, never by summing
//! `f64` amounts (avoids rounding drift). anie-specific: the session
//! total is recomputed from the *current* model's rates, so a mid-session
//! catalog price change re-prices history — accepted to avoid a schema
//! bump (the live meter and `/state` stay self-consistent).

use std::sync::Mutex;

use anie_protocol::{Cost, Message, Usage};
use anie_provider::CostPerMillion;

#[derive(Default)]
struct CostLedger {
    run: Usage,
    session: Usage,
}

/// Run + session token accumulators plus the current pricing.
pub(crate) struct CostMeter {
    ledger: Mutex<CostLedger>,
    pricing: Mutex<CostPerMillion>,
}

/// Point-in-time view for `/state` and the status bar.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CostSnapshot {
    pub run_tokens: u64,
    pub session_tokens: u64,
    pub run_cost: Cost,
    pub session_cost: Cost,
}

fn tokens_of(usage: &Usage) -> u64 {
    usage.input_tokens + usage.output_tokens + usage.cache_read_tokens + usage.cache_write_tokens
}

fn add_into(acc: &mut Usage, usage: &Usage) {
    acc.input_tokens += usage.input_tokens;
    acc.output_tokens += usage.output_tokens;
    acc.cache_read_tokens += usage.cache_read_tokens;
    acc.cache_write_tokens += usage.cache_write_tokens;
}

fn sum_assistant_usage(target: &mut Usage, messages: &[Message]) {
    for message in messages {
        if let Message::Assistant(assistant) = message {
            add_into(target, &assistant.usage);
        }
    }
}

impl CostMeter {
    pub(crate) fn new(pricing: CostPerMillion) -> Self {
        Self {
            ledger: Mutex::new(CostLedger::default()),
            pricing: Mutex::new(pricing),
        }
    }

    /// Update the pricing used for cost derivation (e.g. after `/model`).
    pub(crate) fn set_pricing(&self, pricing: CostPerMillion) {
        *self.lock_pricing() = pricing;
    }

    /// Clear the per-run accumulator at the top of a new user prompt.
    pub(crate) fn reset_run(&self) {
        self.lock_ledger().run = Usage::default();
    }

    /// Fold a completed run's assistant usage into both run and session.
    pub(crate) fn record_run_messages(&self, messages: &[Message]) {
        let mut ledger = self.lock_ledger();
        sum_assistant_usage(&mut ledger.run, messages);
        sum_assistant_usage(&mut ledger.session, messages);
    }

    /// Rebuild the session total from persisted messages (on `--continue`).
    pub(crate) fn rebuild_session(&self, messages: &[Message]) {
        let mut ledger = self.lock_ledger();
        ledger.session = Usage::default();
        sum_assistant_usage(&mut ledger.session, messages);
    }

    pub(crate) fn snapshot(&self) -> CostSnapshot {
        let ledger = self.lock_ledger();
        let pricing = self.lock_pricing();
        CostSnapshot {
            run_tokens: tokens_of(&ledger.run),
            session_tokens: tokens_of(&ledger.session),
            run_cost: pricing.cost_of(&ledger.run),
            session_cost: pricing.cost_of(&ledger.session),
        }
    }

    fn lock_ledger(&self) -> std::sync::MutexGuard<'_, CostLedger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_pricing(&self) -> std::sync::MutexGuard<'_, CostPerMillion> {
        self.pricing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{AssistantMessage, ContentBlock, StopReason};

    fn rates() -> CostPerMillion {
        CostPerMillion {
            input: 3.0,
            output: 15.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }

    fn assistant(input: u64, output: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: "x".into() }],
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
                ..Usage::default()
            },
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "p".into(),
            model: "m".into(),
            timestamp: 1,
            reasoning_details: None,
        })
    }

    #[test]
    fn cost_meter_run_total_resets_at_each_new_prompt() {
        let meter = CostMeter::new(rates());
        meter.record_run_messages(&[assistant(1_000_000, 0)]);
        assert_eq!(meter.snapshot().run_tokens, 1_000_000);
        meter.reset_run();
        assert_eq!(meter.snapshot().run_tokens, 0);
        // Session is unaffected by a run reset.
        assert_eq!(meter.snapshot().session_tokens, 1_000_000);
    }

    #[test]
    fn cost_meter_session_total_accumulates_across_runs() {
        let meter = CostMeter::new(rates());
        meter.record_run_messages(&[assistant(1_000_000, 1_000_000)]);
        meter.reset_run();
        meter.record_run_messages(&[assistant(1_000_000, 0)]);
        let snap = meter.snapshot();
        assert_eq!(snap.session_tokens, 3_000_000);
        // session cost = 2M input * $3 + 1M output * $15 = $6 + $15 = $21.
        assert!((snap.session_cost.total - 21.0).abs() < 1e-9, "{snap:?}");
    }

    #[test]
    fn cost_meter_rebuilds_session_total_from_persisted_usage_on_continue() {
        let meter = CostMeter::new(rates());
        let persisted = vec![assistant(1_000_000, 0), assistant(0, 1_000_000)];
        meter.rebuild_session(&persisted);
        let snap = meter.snapshot();
        assert_eq!(snap.session_tokens, 2_000_000);
        assert!((snap.session_cost.total - 18.0).abs() < 1e-9, "{snap:?}");
    }
}
