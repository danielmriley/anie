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
///
/// | Version | Change                                              |
/// |---------|-----------------------------------------------------|
/// | 1       | Baseline.                                           |
/// | 2       | `tool_repair` counters (coerced / repaired /        |
/// |         | failed_after_repair), sourced from `ToolExecEnd`    |
/// |         | result details. Plan 01 PR 4 of                     |
/// |         | `docs/local_model_augmentation/`. Older artifacts   |
/// |         | load with the counters defaulted to zero.           |
/// | 3       | `tool_repair.repaired_then_failed`, counted off the |
/// |         | `repair_outcome: executed_with_error` detail marker |
/// |         | (a repaired call that then failed at execution).    |
/// |         | Plan 01 §9b. v2 artifacts load with the counter     |
/// |         | defaulted to zero.                                  |
/// | 4       | `recovery` (plan 03 PR 12: verify runs/failures,    |
/// |         | loop perturbations, grounded edit failures) and     |
/// |         | `prompt.system_prompt_tokens` (plan 04 PR 15), one  |
/// |         | combined bump. Older artifacts load with both       |
/// |         | blocks defaulted.                                   |
pub const RUN_METRICS_SCHEMA_VERSION: u32 = 4;

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
    /// Plan 01 of `docs/local_model_augmentation/`: how often
    /// the harness rescued (or failed to rescue) schema-invalid
    /// tool calls. `#[serde(default)]` so schema-v1 artifacts
    /// still deserialize.
    #[serde(default)]
    pub tool_repair: ToolRepairMetrics,
    /// Plan 03 PR 12 of `docs/local_model_augmentation/`:
    /// write-side recovery counters (schema v4). Defaulted so
    /// older artifacts still deserialize.
    #[serde(default)]
    pub recovery: RecoveryMetrics,
    /// Plan 04 PR 15 of `docs/local_model_augmentation/`:
    /// prompt-weight metrics (schema v4). Defaulted so older
    /// artifacts still deserialize.
    #[serde(default)]
    pub prompt: PromptMetrics,
}

/// Counters for the plan-03 write-side recovery pipeline, derived
/// from signals already on the event stream:
///
/// - `verify_runs` / `verify_failures`: the harness verify policy's
///   `SystemMessage` events (`verify_runner::VERIFY_EVENT_PREFIX` /
///   `VERIFY_FAILURE_EVENT_PREFIX`).
/// - `loop_perturbations`: the `[loop warning]` `SystemMessage` the
///   failure-loop detector emits at the same threshold crossing that
///   arms the temperature perturbation (`agent_loop.rs::
///   observe_failure_loop`). anie-specific caveat: a similar-call
///   streak of 5 also arms the shared slot but emits no event, and
///   `ANIE_LOOP_PERTURB=0` keeps the warning while disabling the
///   bump — so this counts detector crossings, the observable
///   signal, not literal temperature bumps.
/// - `grounded_edit_failures`: tool results carrying the grounding
///   attachment (`agent_loop.rs::edit_grounding_note`'s stable
///   prefix).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RecoveryMetrics {
    /// Harness-run verify command executions (plan 03 §2a).
    pub verify_runs: u32,
    /// Verify executions that failed (non-zero exit, timeout, or
    /// spawn failure).
    pub verify_failures: u32,
    /// Failure-loop threshold crossings that arm the temperature
    /// perturbation (plan 03 §2b; see the caveat above).
    pub loop_perturbations: u32,
    /// Edit/apply_patch failures that received a grounded
    /// current-content attachment (plan 03 §2c).
    pub grounded_edit_failures: u32,
}

/// Prompt-weight metrics (plan 04 §2e / PR 15).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PromptMetrics {
    /// Estimated tokens in the composed system prompt (the same
    /// bytes/4 yardstick as `anie_session::estimate_tokens`'s text
    /// rule). `0` when the producer didn't report it.
    pub system_prompt_tokens: u64,
}

/// Stable prefix of the failure-loop detector's SystemMessage
/// (`agent_loop.rs::observe_failure_loop`). The detector lives in
/// `anie-agent`, which doesn't export the literal — the drift risk
/// is accepted and guarded by the agent-side tests on the message
/// text.
const LOOP_WARNING_EVENT_PREFIX: &str = "[loop warning]";

/// Stable prefix of the grounded edit-failure attachment
/// (`agent_loop.rs::edit_grounding_note`).
const EDIT_GROUNDING_NOTE_PREFIX: &str = "[harness note] Current content of ";

/// Counters for the Plan-01 tool-call rescue pipeline, derived
/// from markers the agent loop leaves in `ToolExecEnd` result
/// details (`argument_coercions`, `argument_repair_rounds`,
/// `argument_repair_failed`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolRepairMetrics {
    /// Calls fixed by deterministic schema-guided coercion.
    pub coerced: u32,
    /// Calls fixed by the generative repair round.
    pub repaired: u32,
    /// Calls where repair was attempted and the call still
    /// failed validation.
    pub failed_after_repair: u32,
    /// Calls the repair round made schema-valid that then failed
    /// at EXECUTION (repaired-but-worse; schema v3). Plan 01 §9b
    /// of `docs/local_model_augmentation/`.
    #[serde(default)]
    pub repaired_then_failed: u32,
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
    tool_repair: ToolRepairMetrics,
    recovery: RecoveryMetrics,
    prompt: PromptMetrics,
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
            tool_repair: ToolRepairMetrics::default(),
            recovery: RecoveryMetrics::default(),
            prompt: PromptMetrics::default(),
            pending_tools: HashMap::new(),
        }
    }

    /// Record the composed system prompt's estimated weight (plan 04
    /// PR 15). Call once, at construction time, with the same prompt
    /// the run sends — `run_print_mode` does so right after `new`.
    pub fn set_system_prompt(&mut self, system_prompt: &str) {
        self.prompt.system_prompt_tokens = crate::controller::estimate_text_tokens(system_prompt);
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
                call_id,
                result,
                is_error,
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
                // Plan 01 PR 4: the agent loop marks rescued (or
                // unrescuable) calls in the result details.
                if result.details.get("argument_coercions").is_some() {
                    self.tool_repair.coerced += 1;
                }
                if result.details.get("argument_repair_rounds").is_some() {
                    self.tool_repair.repaired += 1;
                }
                if result.details.get("argument_repair_failed").is_some() {
                    self.tool_repair.failed_after_repair += 1;
                }
                // Schema v3 (Plan 01 §9b): repaired-but-worse — the
                // repaired call executed and failed.
                if result
                    .details
                    .get("repair_outcome")
                    .and_then(serde_json::Value::as_str)
                    == Some("executed_with_error")
                {
                    self.tool_repair.repaired_then_failed += 1;
                }
                // Schema v4 (plan 03 §2c): a grounded edit failure
                // carries the current-content attachment in the
                // result.
                if result.content.iter().any(|block| {
                    matches!(block, anie_protocol::ContentBlock::Text { text }
                        if text.starts_with(EDIT_GROUNDING_NOTE_PREFIX))
                }) {
                    self.recovery.grounded_edit_failures += 1;
                }
            }
            // Schema v4 (plan 03 §2a/§2b): verify runs and
            // loop-perturbation crossings surface as SystemMessage
            // events with stable prefixes.
            AgentEvent::SystemMessage { text } => {
                if text.starts_with(crate::verify_runner::VERIFY_EVENT_PREFIX) {
                    self.recovery.verify_runs += 1;
                    if text.starts_with(crate::verify_runner::VERIFY_FAILURE_EVENT_PREFIX) {
                        self.recovery.verify_failures += 1;
                    }
                } else if text.starts_with(LOOP_WARNING_EVENT_PREFIX) {
                    self.recovery.loop_perturbations += 1;
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
            tool_repair: self.tool_repair,
            recovery: self.recovery,
            prompt: self.prompt,
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
    fn run_metrics_json_roundtrips_with_current_schema_version() {
        let m = fold(&[assistant(Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..Usage::default()
        })]);
        let json = serde_json::to_string(&m).expect("serialize");
        let back: RunMetrics = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back);
        assert_eq!(back.schema_version, RUN_METRICS_SCHEMA_VERSION);
    }

    fn tool_end_with_details(call_id: &str, is_error: bool, details: serde_json::Value) -> AgentEvent {
        AgentEvent::ToolExecEnd {
            call_id: call_id.into(),
            result: ToolResult {
                content: vec![],
                details,
            },
            is_error,
        }
    }

    /// Plan 01 PR 4: the accumulator derives rescue counters
    /// from the detail markers the agent loop attaches —
    /// coerced, repaired, and failed-after-repair each count
    /// independently; unmarked calls count nothing.
    #[test]
    fn run_metrics_reports_coerced_and_repaired_counts() {
        let m = fold(&[
            tool_start("c1", "read"),
            tool_end_with_details(
                "c1",
                false,
                serde_json::json!({"argument_coercions": ["limit: coerced"]}),
            ),
            tool_start("c2", "edit"),
            tool_end_with_details("c2", false, serde_json::json!({"argument_repair_rounds": 1})),
            tool_start("c3", "edit"),
            tool_end_with_details("c3", true, serde_json::json!({"argument_repair_failed": true})),
            tool_start("c4", "bash"),
            tool_end("c4", false),
        ]);
        assert_eq!(m.tool_repair.coerced, 1);
        assert_eq!(m.tool_repair.repaired, 1);
        assert_eq!(m.tool_repair.failed_after_repair, 1);
        assert_eq!(m.tool_repair.repaired_then_failed, 0);
        assert_eq!(m.tools.calls, 4, "rescue counters don't replace call counts");
    }

    /// Schema v3 (Plan 01 §9b): a repaired call that then failed
    /// at execution carries both the repair note and the
    /// `repair_outcome` marker; the accumulator counts it under
    /// `repaired` AND `repaired_then_failed` (it was repaired —
    /// the repair just didn't help).
    #[test]
    fn repaired_then_failed_counts_off_the_execution_outcome_marker() {
        let m = fold(&[
            tool_start("c1", "bash"),
            tool_end_with_details(
                "c1",
                true,
                serde_json::json!({
                    "argument_repair_rounds": 1,
                    "repair_outcome": "executed_with_error"
                }),
            ),
        ]);
        assert_eq!(m.tool_repair.repaired, 1);
        assert_eq!(m.tool_repair.repaired_then_failed, 1);
        assert_eq!(m.tool_repair.failed_after_repair, 0);
    }

    /// Forward-compat: a schema-v1 artifact (no `tool_repair`
    /// field) loads with the counters defaulted, not an error.
    #[test]
    fn older_metrics_schema_loads_with_repair_counters_defaulted() {
        let v1 = serde_json::json!({
            "schema_version": 1,
            "harness_mode": "current",
            "model": "m",
            "provider": "p",
            "wall_clock_ms": 5,
            "turns": 1,
            "tokens": TokenMetrics::default(),
            "cost": Cost::default(),
            "tools": ToolMetrics::default(),
            "compaction": CompactionMetrics::default(),
        });
        let back: RunMetrics = serde_json::from_value(v1).expect("v1 artifact loads");
        assert_eq!(back.tool_repair, ToolRepairMetrics::default());
    }

    /// Forward-compat: a schema-v2 artifact (a `tool_repair`
    /// block without `repaired_then_failed`) loads with the new
    /// counter defaulted, not an error.
    #[test]
    fn v2_metrics_schema_loads_with_repaired_then_failed_defaulted() {
        let v2 = serde_json::json!({
            "schema_version": 2,
            "harness_mode": "rlm",
            "model": "m",
            "provider": "p",
            "wall_clock_ms": 5,
            "turns": 1,
            "tokens": TokenMetrics::default(),
            "cost": Cost::default(),
            "tools": ToolMetrics::default(),
            "compaction": CompactionMetrics::default(),
            "tool_repair": {
                "coerced": 4,
                "repaired": 3,
                "failed_after_repair": 1
            },
        });
        let back: RunMetrics = serde_json::from_value(v2).expect("v2 artifact loads");
        assert_eq!(back.tool_repair.repaired, 3);
        assert_eq!(back.tool_repair.repaired_then_failed, 0);
    }

    /// Plan 03 PR 12 (schema v4): recovery counters fold off the
    /// observable signals — verify SystemMessages, loop-warning
    /// SystemMessages, and the grounded-content attachment in tool
    /// results. Unrelated SystemMessages count nothing.
    #[test]
    fn run_metrics_reports_recovery_counters() {
        let grounded = AgentEvent::ToolExecEnd {
            call_id: "c1".into(),
            result: ToolResult {
                content: vec![ContentBlock::Text {
                    text: format!("{EDIT_GROUNDING_NOTE_PREFIX}src/foo.rs:120-180 ..."),
                }],
                details: serde_json::Value::Null,
            },
            is_error: true,
        };
        let m = fold(&[
            AgentEvent::SystemMessage {
                text: "[verify] passed: cargo check".into(),
            },
            AgentEvent::SystemMessage {
                text: "[verify] FAILED (exit 101): cargo check".into(),
            },
            AgentEvent::SystemMessage {
                text: "[loop warning] tool `edit` failed 3 times in a row ...".into(),
            },
            AgentEvent::SystemMessage {
                text: "switched model to x".into(),
            },
            tool_start("c1", "edit"),
            grounded,
        ]);
        assert_eq!(m.recovery.verify_runs, 2);
        assert_eq!(m.recovery.verify_failures, 1);
        assert_eq!(m.recovery.loop_perturbations, 1);
        assert_eq!(m.recovery.grounded_edit_failures, 1);
    }

    /// Forward-compat: a schema-v3 artifact (no `recovery` /
    /// `prompt` blocks) loads with both defaulted, not an error.
    #[test]
    fn older_metrics_schema_loads_with_recovery_defaulted() {
        let v3 = serde_json::json!({
            "schema_version": 3,
            "harness_mode": "rlm",
            "model": "m",
            "provider": "p",
            "wall_clock_ms": 5,
            "turns": 1,
            "tokens": TokenMetrics::default(),
            "cost": Cost::default(),
            "tools": ToolMetrics::default(),
            "compaction": CompactionMetrics::default(),
            "tool_repair": ToolRepairMetrics::default(),
        });
        let back: RunMetrics = serde_json::from_value(v3).expect("v3 artifact loads");
        assert_eq!(back.recovery, RecoveryMetrics::default());
        assert_eq!(back.prompt, PromptMetrics::default());
    }

    /// Plan 04 PR 15: the prompt-weight estimate uses the bytes/4
    /// yardstick shared with the eviction pipeline.
    #[test]
    fn system_prompt_tokens_use_the_bytes_over_four_yardstick() {
        let mut a = acc();
        a.set_system_prompt(&"x".repeat(400));
        let m = a.finish();
        assert_eq!(m.prompt.system_prompt_tokens, 100);
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
