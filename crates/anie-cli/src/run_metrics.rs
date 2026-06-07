//! Per-run metrics export (`--metrics-out`).
//!
//! Aggregates a print-mode run's `AgentEvent` stream into a small,
//! serializable [`RunMetrics`] sidecar: token usage, latency, tool
//! success/failure (overall + per tool), cost, and compaction counts by
//! phase. Every field is sourced from data already on the event stream,
//! so nothing new is threaded out of the agent core. The accumulator is
//! pure (no I/O) so it unit-tests against a synthetic event vector.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use anie_protocol::{AgentEvent, CompactionPhase, Cost, Message};

use crate::harness_mode::HarnessMode;

/// Schema version for the metrics artifact. Independent of the session
/// schema — this is a sidecar file, not a persisted session type.
pub const RUN_METRICS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMetrics {
    pub schema_version: u32,
    pub harness_mode: String,
    pub model: String,
    pub provider: String,
    pub wall_clock_ms: u64,
    pub turns: u32,
    pub tokens: TokenMetrics,
    /// Cost (USD, estimated). Now populated for priced models (cost
    /// initiative); `0.0` for local/free models — exported either way, so
    /// the runner must not treat `0` as missing.
    pub cost: Cost,
    pub tools: ToolMetrics,
    pub compaction: CompactionMetrics,
}

/// Field names mirror `Usage` exactly. `total_tokens` here is the summed
/// export total (provider-reported when available, else input+output).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TokenMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolMetrics {
    pub calls: u32,
    pub failures: u32,
    /// Per-tool outcomes keyed by tool name (BTreeMap for stable
    /// serialization order).
    pub by_tool: BTreeMap<String, ToolOutcome>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolOutcome {
    pub calls: u32,
    pub failures: u32,
}

/// Export-facing twin of `CompactionStats`. Counts events in the sink
/// (rather than reading the in-process atomic) so the metric is
/// self-contained in the print-mode consumer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CompactionMetrics {
    pub pre_prompt: u32,
    pub mid_turn: u32,
    pub reactive_overflow: u32,
    pub total: u32,
}

/// Folds a run's events into the running totals.
pub struct RunMetricsAccumulator {
    started: Instant,
    harness_mode: String,
    model: String,
    provider: String,
    turns: u32,
    tokens: TokenMetrics,
    cost: Cost,
    tools: ToolMetrics,
    compaction: CompactionMetrics,
    /// `call_id -> tool_name`, seeded from `ToolExecStart`, so the
    /// nameless `ToolExecEnd` can attribute per-tool outcomes.
    pending_tools: HashMap<String, String>,
}

impl RunMetricsAccumulator {
    #[must_use]
    pub fn new(mode: HarnessMode, model: &str, provider: &str) -> Self {
        Self {
            started: Instant::now(),
            harness_mode: mode.label().to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
            turns: 0,
            tokens: TokenMetrics::default(),
            cost: Cost::default(),
            tools: ToolMetrics::default(),
            compaction: CompactionMetrics::default(),
            pending_tools: HashMap::new(),
        }
    }

    /// Fold one event into the running totals. No I/O.
    pub fn observe(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageEnd {
                message: Message::Assistant(assistant),
            } => {
                let usage = &assistant.usage;
                self.tokens.input_tokens += usage.input_tokens;
                self.tokens.output_tokens += usage.output_tokens;
                self.tokens.cache_read_tokens += usage.cache_read_tokens;
                self.tokens.cache_write_tokens += usage.cache_write_tokens;
                // Prefer the provider-reported total when present, else
                // input+output (don't double-count cache tiers).
                self.tokens.total_tokens += usage
                    .total_tokens
                    .unwrap_or(usage.input_tokens + usage.output_tokens);
                add_cost(&mut self.cost, &usage.cost);
                self.turns += 1;
            }
            AgentEvent::ToolExecStart {
                call_id, tool_name, ..
            } => {
                self.pending_tools
                    .insert(call_id.clone(), tool_name.clone());
            }
            AgentEvent::ToolExecEnd {
                call_id, is_error, ..
            } => {
                // ToolExecEnd carries no tool_name (anie-specific: the
                // name is only on ToolExecStart), so attribute via the
                // call_id map; default to "<unknown>" defensively.
                let name = self
                    .pending_tools
                    .remove(call_id)
                    .unwrap_or_else(|| "<unknown>".to_string());
                self.tools.calls += 1;
                let outcome = self.tools.by_tool.entry(name).or_default();
                outcome.calls += 1;
                if *is_error {
                    self.tools.failures += 1;
                    outcome.failures += 1;
                }
            }
            AgentEvent::CompactionEnd { phase, .. } => {
                self.compaction.total += 1;
                match phase {
                    CompactionPhase::PrePrompt => self.compaction.pre_prompt += 1,
                    CompactionPhase::MidTurn => self.compaction.mid_turn += 1,
                    CompactionPhase::ReactiveOverflow => self.compaction.reactive_overflow += 1,
                }
            }
            _ => {}
        }
    }

    /// Snapshot into the serializable artifact.
    #[must_use]
    pub fn finish(self) -> RunMetrics {
        RunMetrics {
            schema_version: RUN_METRICS_SCHEMA_VERSION,
            harness_mode: self.harness_mode,
            model: self.model,
            provider: self.provider,
            wall_clock_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            turns: self.turns,
            tokens: self.tokens,
            cost: self.cost,
            tools: self.tools,
            compaction: self.compaction,
        }
    }
}

fn add_cost(acc: &mut Cost, cost: &Cost) {
    acc.input += cost.input;
    acc.output += cost.output;
    acc.cache_read += cost.cache_read;
    acc.cache_write += cost.cache_write;
    acc.total += cost.total;
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{AssistantMessage, ContentBlock, StopReason, ToolResult, Usage};

    fn assistant(usage: Usage) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text { text: "x".into() }],
                usage,
                stop_reason: StopReason::Stop,
                error_message: None,
                provider: "p".into(),
                model: "m".into(),
                timestamp: 1,
                reasoning_details: None,
            }),
        }
    }

    fn tool_start(call_id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolExecStart {
            call_id: call_id.into(),
            tool_name: name.into(),
            args: serde_json::Value::Null,
        }
    }

    fn tool_end(call_id: &str, is_error: bool) -> AgentEvent {
        AgentEvent::ToolExecEnd {
            call_id: call_id.into(),
            result: ToolResult {
                content: vec![],
                details: serde_json::Value::Null,
            },
            is_error,
        }
    }

    fn compaction(phase: CompactionPhase) -> AgentEvent {
        AgentEvent::CompactionEnd {
            phase,
            summary: String::new(),
            tokens_before: 0,
            tokens_after: 0,
        }
    }

    fn acc() -> RunMetricsAccumulator {
        RunMetricsAccumulator::new(HarnessMode::default(), "m", "p")
    }

    fn fold(events: &[AgentEvent]) -> RunMetrics {
        let mut a = acc();
        for e in events {
            a.observe(e);
        }
        a.finish()
    }

    #[test]
    fn accumulator_sums_token_usage_across_assistant_messages() {
        let m = fold(&[
            assistant(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            }),
            assistant(Usage {
                input_tokens: 20,
                output_tokens: 7,
                ..Usage::default()
            }),
        ]);
        assert_eq!(m.tokens.input_tokens, 30);
        assert_eq!(m.tokens.output_tokens, 12);
        assert_eq!(m.turns, 2);
    }

    #[test]
    fn accumulator_prefers_reported_total_tokens_over_input_plus_output() {
        let m = fold(&[assistant(Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: Some(99),
            ..Usage::default()
        })]);
        assert_eq!(m.tokens.total_tokens, 99);
    }

    #[test]
    fn tool_failure_rate_counts_is_error_ends_and_attributes_by_tool_name() {
        let m = fold(&[
            tool_start("c1", "grep"),
            tool_end("c1", false),
            tool_start("c2", "grep"),
            tool_end("c2", true),
            tool_start("c3", "read"),
            tool_end("c3", false),
        ]);
        assert_eq!(m.tools.calls, 3);
        assert_eq!(m.tools.failures, 1);
        assert_eq!(
            m.tools.by_tool["grep"],
            ToolOutcome {
                calls: 2,
                failures: 1
            }
        );
        assert_eq!(
            m.tools.by_tool["read"],
            ToolOutcome {
                calls: 1,
                failures: 0
            }
        );
    }

    #[test]
    fn tool_outcome_attribution_uses_call_id_from_exec_start() {
        // An End with no matching Start attributes to "<unknown>" rather
        // than being dropped.
        let m = fold(&[tool_end("orphan", true)]);
        assert_eq!(m.tools.calls, 1);
        assert_eq!(m.tools.by_tool["<unknown>"].failures, 1);
    }

    #[test]
    fn compaction_metrics_count_each_phase_from_compaction_end_events() {
        let m = fold(&[
            compaction(CompactionPhase::PrePrompt),
            compaction(CompactionPhase::MidTurn),
            compaction(CompactionPhase::MidTurn),
            compaction(CompactionPhase::ReactiveOverflow),
        ]);
        assert_eq!(m.compaction.pre_prompt, 1);
        assert_eq!(m.compaction.mid_turn, 2);
        assert_eq!(m.compaction.reactive_overflow, 1);
        assert_eq!(m.compaction.total, 4);
    }

    #[test]
    fn compaction_metrics_match_compaction_stats_atomic_on_same_event_stream() {
        let events = [
            compaction(CompactionPhase::PrePrompt),
            compaction(CompactionPhase::MidTurn),
            compaction(CompactionPhase::ReactiveOverflow),
        ];
        let m = fold(&events);
        // The in-process atomic, fed the same phases, must agree.
        let atomic = crate::compaction_stats::CompactionStatsAtomic::default();
        for e in &events {
            if let AgentEvent::CompactionEnd { phase, .. } = e {
                atomic.increment(*phase);
            }
        }
        let snap = atomic.snapshot();
        assert_eq!(m.compaction.pre_prompt, snap.pre_prompt);
        assert_eq!(m.compaction.mid_turn, snap.mid_turn);
        assert_eq!(m.compaction.reactive_overflow, snap.reactive_overflow);
        assert_eq!(m.compaction.total, snap.total());
    }

    #[test]
    fn run_metrics_json_roundtrips_with_schema_version_1() {
        let m = fold(&[assistant(Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..Usage::default()
        })]);
        let json = serde_json::to_string(&m).expect("serialize");
        let back: RunMetrics = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
        assert_eq!(back.schema_version, 1);
    }

    #[test]
    fn cost_total_zero_is_exported_not_treated_as_missing() {
        // A local/free run has zero cost; the field is still present.
        let m = fold(&[assistant(Usage {
            input_tokens: 100,
            output_tokens: 100,
            ..Usage::default()
        })]);
        assert_eq!(m.cost, Cost::default());
        let json = serde_json::to_value(&m).expect("json");
        assert!(json.get("cost").is_some(), "cost field is always exported");
    }
}
