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

use anie_protocol::{AgentEvent, CompactionPhase, Cost, EditGuardSignal, Message};

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
/// | 5       | `context` block (rlm2/PR1: eviction + page-in       |
/// |         | deltas folded off `RlmStatsUpdate`, Ollama prefill  |
/// |         | totals off `prompt_eval_count`, and the             |
/// |         | silent-truncation suspicion counter). Older         |
/// |         | artifacts load with the block defaulted.            |
/// | 6       | `edit_guard` block (edit-completion guard, PR 3 of  |
/// |         | `docs/edit_completion_guard/`): the classifier      |
/// |         | verdict, guard-fire count, rounds spent, and edits  |
/// |         | made after a fire — all folded off                  |
/// |         | `AgentEvent::EditGuard` and the mutating-tool       |
/// |         | `ToolExecEnd`s that follow a fire. Older artifacts  |
/// |         | load with the block defaulted.                      |
pub const RUN_METRICS_SCHEMA_VERSION: u32 = 6;

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
    /// rlm2/PR1 of `docs/rlm_context_v2/`: context-virtualization
    /// instrumentation (schema v5). Defaulted so older artifacts
    /// still deserialize.
    #[serde(default)]
    pub context: ContextMetrics,
    /// PR 3 of `docs/edit_completion_guard/`: edit-completion-guard
    /// instrumentation (schema v6). Defaulted so older artifacts
    /// still deserialize.
    #[serde(default)]
    pub edit_guard: EditGuardMetrics,
}

/// Edit-completion-guard telemetry (PR 3 of
/// `docs/edit_completion_guard/`), folded off
/// `AgentEvent::EditGuard` plus the mutating-tool `ToolExecEnd`s
/// that follow a fire:
///
/// - `classified_expected`: the model-judged classifier's verdict,
///   when it ran. `None` when an explicit `--require-edit` /
///   `[guard].require_edit` override short-circuited the classifier
///   (no classification was performed), so `None` is meaningfully
///   distinct from a recorded `Some(false)` and is skipped on
///   serialize. Set at most once per run (the classifier is
///   one-shot and cached).
/// - `guard_fired`: number of `EditGuardSignal::Fired` events — how
///   many times the guard engaged and injected a directive.
/// - `guard_rounds`: total budget rounds spent across those fires.
///   Each fire spends exactly one round today, so this tracks
///   `guard_fired`; kept distinct because the round count is the
///   budget-facing number the spec asks to measure separately.
/// - `edit_after_guard`: successful file-mutating tool calls
///   (`edit` / `write` / `apply_patch`) observed AFTER the first
///   guard fire — the signal that the intervention actually pushed
///   the model into editing. Mutations before any fire don't count
///   (the guard never engages once a mutation has happened).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EditGuardMetrics {
    /// The classifier verdict, when the classifier ran. `None` (and
    /// omitted from JSON) when an explicit override skipped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified_expected: Option<bool>,
    /// Guard-fire count (`EditGuardSignal::Fired` events).
    pub guard_fired: u32,
    /// Total budget rounds spent across the fires.
    pub guard_rounds: u32,
    /// Successful file mutations observed after the first fire.
    pub edit_after_guard: u32,
}

/// Context-virtualization telemetry (rlm2/PR1). Folds the per-fire
/// deltas the rlm policy attaches to `AgentEvent::RlmStatsUpdate`
/// plus Ollama prefill signals off assistant `Usage`:
///
/// - `evictions` / `evicted_tokens` / `page_ins` / `page_in_tokens` /
///   `ledger_tokens_total`: summed over every `RlmStatsUpdate` fire
///   (one per turn under rlm). Zero on non-rlm runs, which emit no
///   such event.
/// - `prefill_tokens_total`: sums Ollama's per-turn
///   `prompt_eval_count` (mapped onto `Usage::input_tokens`). Counted
///   only for the `ollama` provider — hosted providers don't carry
///   prefill semantics, and a turn's input tokens there already live
///   in `tokens.input_tokens`.
/// - `truncation_suspected`: increments when a turn's Ollama
///   `prompt_eval_count` carries the context-shift signature: the
///   rlm policy's sent estimate (`sent_context_tokens`) exceeded the
///   effective `num_ctx`, and the prefill count landed near the
///   window yet under `TRUNCATION_FLOOR_FACTOR` × the estimate —
///   Ollama re-evaluated a shifted window rather than prefilling
///   what we sent (the P1 bug class PR2 alarms on). A prefill far
///   *below* the window is a healthy prefix-cache hit (Ollama's
///   `prompt_eval_count` counts only newly-evaluated tokens), never
///   a truncation. Never fires for non-Ollama providers (no
///   `prompt_eval_count` semantics), never without a measured send,
///   and never off an errored/aborted reply.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ContextMetrics {
    pub evictions: u64,
    pub evicted_tokens: u64,
    pub page_ins: u64,
    pub page_in_tokens: u64,
    pub ledger_tokens_total: u64,
    pub prefill_tokens_total: u64,
    pub truncation_suspected: u64,
}

/// The fraction of our sent-context estimate that Ollama's
/// `prompt_eval_count` must clear to be considered un-truncated.
/// `estimate_tokens` is a bytes/4 heuristic, so the prefill count
/// legitimately drifts from it (real tokenizers pack denser on code,
/// looser on prose); `0.9` is wide enough to absorb that band while
/// still catching the live bug, where context-shift discards the
/// whole system prompt + oldest turns and prefill comes in far below
/// our estimate. PR2 consumes this signal for its alarm.
const TRUNCATION_FLOOR_FACTOR: f64 = 0.9;

/// The fraction of the effective `num_ctx` a prefill count must
/// reach before an undershoot is read as a context shift rather
/// than a prefix-cache hit. Ollama's `prompt_eval_count` counts
/// only tokens evaluated THIS request: on a healthy append-only
/// turn the cached prefix covers everything but the new suffix and
/// the count comes in tiny. A real context shift misaligns the
/// cached prefix and forces a near-full re-eval of the shifted
/// window, so prefill lands near `num_ctx`. Requiring at least
/// half the window separates the two regimes with room for
/// partial-cache edge cases.
const TRUNCATION_NEAR_CTX_FACTOR: f64 = 0.5;

/// Provider string that carries `prompt_eval_count` prefill semantics
/// (`Usage::input_tokens` is Ollama's `prompt_eval_count`). The
/// truncation detector and prefill total gate on this so hosted
/// providers are never touched. Shared with the rlm policy's
/// truncation alarm (rlm2/PR2, `context_virt.rs`).
pub(crate) const OLLAMA_PROVIDER: &str = "ollama";

/// The PR1 truncation-detector predicate: did Ollama's
/// `prompt_eval_count` (`prefill_tokens`) come back with the
/// context-shift signature? Shared between the metrics accumulator
/// (counts `truncation_suspected`) and the rlm policy's user-facing
/// alarm (rlm2/PR2) so the two surfaces can never disagree on what
/// counts as a truncation.
///
/// `prompt_eval_count` counts only tokens Ollama evaluated THIS
/// request — a prefix-cache hit legitimately reports just the new
/// suffix (or omits the field entirely when the whole prompt was
/// cached). So a bare undershoot of the sent estimate is NOT a
/// truncation; the shift signature is all of:
///
/// - the sent estimate exceeded the effective `num_ctx` (a shift is
///   possible at all),
/// - prefill landed near the window
///   ([`TRUNCATION_NEAR_CTX_FACTOR`] × `num_ctx` — the shifted
///   prefix misaligns the cache, forcing a near-full re-eval),
/// - and prefill still undershot [`TRUNCATION_FLOOR_FACTOR`] × the
///   sent estimate.
///
/// Without a known `num_ctx` (hosted providers, Ollama through a
/// compat API) the two regimes are indistinguishable, so the
/// detector stays off. A prefill of 0 (field omitted: fully-cached
/// prompt, or an errored reply that never reached the model) never
/// flags.
pub(crate) fn prefill_indicates_truncation(
    sent_context_tokens: u64,
    prefill_tokens: u64,
    ollama_num_ctx: Option<u64>,
) -> bool {
    if sent_context_tokens == 0 || prefill_tokens == 0 {
        return false;
    }
    let Some(num_ctx) = ollama_num_ctx else {
        return false;
    };
    if sent_context_tokens <= num_ctx {
        return false;
    }
    #[allow(clippy::cast_precision_loss)]
    let near_ctx = TRUNCATION_NEAR_CTX_FACTOR * num_ctx as f64;
    if (prefill_tokens as f64) < near_ctx {
        // Far below the window: a prefix-cache hit, not a shift.
        return false;
    }
    #[allow(clippy::cast_precision_loss)]
    let floor = TRUNCATION_FLOOR_FACTOR * sent_context_tokens as f64;
    (prefill_tokens as f64) < floor
}

/// Did this assistant message come from a completed model turn?
/// The agent loop synthesizes assistant messages for preflight /
/// stream failures (`error_assistant_message`) and aborted streams
/// with the run's real provider string and a fresh timestamp —
/// those never evaluated the send, so their usage is not a prefill
/// sample. Shared by the truncation detector here and the rlm
/// policy's alarm (`context_virt.rs`).
pub(crate) fn is_completed_reply(reply: &anie_protocol::AssistantMessage) -> bool {
    reply.error_message.is_none()
        && !matches!(
            reply.stop_reason,
            anie_protocol::StopReason::Error | anie_protocol::StopReason::Aborted
        )
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

/// Tool names whose successful execution counts as a file mutation
/// for the edit-completion guard. Mirrors `MUTATING_TOOLS` in both
/// `anie-agent/src/agent_loop.rs` and `anie-cli/src/verify_runner.rs`
/// — the same three tools. The literals live in `anie-agent`, which
/// doesn't export them; the drift risk is accepted and guarded by
/// the agent-side guard tests on the same set.
const MUTATING_TOOLS: [&str; 3] = ["edit", "write", "apply_patch"];

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
    context: ContextMetrics,
    edit_guard: EditGuardMetrics,
    /// Whether the edit-completion guard has fired at least once.
    /// `edit_after_guard` only counts mutations observed once this
    /// is set, so a mutation before any fire (impossible in practice
    /// — the guard never engages after a mutation — but cheap to be
    /// precise about) is never miscounted as guard-attributable.
    guard_has_fired: bool,
    /// The `sent_context_tokens` from the most recent `RlmStatsUpdate`,
    /// pending comparison against the NEXT turn's Ollama
    /// `prompt_eval_count`. `None` until the rlm policy reports a send;
    /// reset after each comparison so a turn without a fresh
    /// `RlmStatsUpdate` (non-rlm runs, or the rare fire-less turn)
    /// can't false-alarm off a stale estimate.
    pending_sent_context_tokens: Option<u64>,
    /// The effective `num_ctx` the run requests from Ollama, when
    /// known (`Some` only for native Ollama chat models). The
    /// truncation detector needs it to tell a context shift from a
    /// prefix-cache hit; `None` keeps the detector off.
    ollama_num_ctx: Option<u64>,
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
            context: ContextMetrics::default(),
            edit_guard: EditGuardMetrics::default(),
            guard_has_fired: false,
            pending_sent_context_tokens: None,
            ollama_num_ctx: None,
            pending_tools: HashMap::new(),
        }
    }

    /// Record the effective Ollama context window (`num_ctx`) the
    /// run will request — `Some` only for native Ollama chat
    /// models. Call once at construction time (alongside
    /// [`Self::set_system_prompt`]); without it the truncation
    /// detector stays off, because a prefill undershoot can't be
    /// told apart from a healthy prefix-cache hit.
    pub fn set_ollama_num_ctx(&mut self, num_ctx: Option<u64>) {
        self.ollama_num_ctx = num_ctx;
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

                // rlm2/PR1: prefill telemetry + silent-truncation
                // detection, Ollama-only. `Usage::input_tokens` is
                // Ollama's `prompt_eval_count` — the count of context
                // tokens it actually prefilled this turn.
                if assistant.provider == OLLAMA_PROVIDER {
                    self.context.prefill_tokens_total += usage.input_tokens;
                    // Compare against the context the rlm policy
                    // reported sending for THIS turn (if any). The
                    // shift signature (sent above num_ctx, prefill
                    // near num_ctx yet below the estimate) means
                    // Ollama context-shifted instead of prefilling
                    // what we sent. `take()` clears the estimate so
                    // a later turn without a fresh `RlmStatsUpdate`
                    // can't re-fire off it. Error/abort-shaped
                    // replies never evaluated the send, so they are
                    // not prefill samples.
                    if let Some(sent) = self.pending_sent_context_tokens.take() {
                        if is_completed_reply(assistant)
                            && prefill_indicates_truncation(
                                sent,
                                usage.input_tokens,
                                self.ollama_num_ctx,
                            )
                        {
                            self.context.truncation_suspected += 1;
                        }
                    }
                }
            }
            // rlm2/PR1: fold the rlm policy's per-fire deltas into the
            // context block and stash the sent-context estimate for the
            // next turn's truncation check.
            AgentEvent::RlmStatsUpdate {
                evicted_count,
                evicted_tokens,
                paged_in_count,
                paged_in_tokens,
                ledger_tokens,
                sent_context_tokens,
                ..
            } => {
                self.context.evictions += *evicted_count;
                self.context.evicted_tokens += *evicted_tokens;
                self.context.page_ins += *paged_in_count;
                self.context.page_in_tokens += *paged_in_tokens;
                self.context.ledger_tokens_total += *ledger_tokens;
                if *sent_context_tokens > 0 {
                    self.pending_sent_context_tokens = Some(*sent_context_tokens);
                }
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
                // PR 3 (edit_completion_guard): a successful file
                // mutation after the guard has fired is the signal
                // the intervention worked. Checked before `name` is
                // moved into the per-tool map below.
                if self.guard_has_fired
                    && !*is_error
                    && MUTATING_TOOLS.contains(&name.as_str())
                {
                    self.edit_guard.edit_after_guard += 1;
                }
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
            // PR 3 (edit_completion_guard): fold the guard's
            // classifier verdict and each fire. `Fired` arms
            // `guard_has_fired` so subsequent mutating-tool ends
            // count toward `edit_after_guard`.
            AgentEvent::EditGuard { signal } => match signal {
                EditGuardSignal::Classified(verdict) => {
                    self.edit_guard.classified_expected = Some(*verdict);
                }
                EditGuardSignal::Fired { .. } => {
                    self.edit_guard.guard_fired += 1;
                    self.edit_guard.guard_rounds += 1;
                    self.guard_has_fired = true;
                }
            },
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

    /// Snapshot the running totals without consuming the accumulator.
    /// Used for incremental, crash-safe sidecar flushes during a run
    /// (a killed run — e.g. a benchmark wall-clock timeout — then still
    /// leaves its latest metrics on disk instead of nothing).
    #[must_use]
    pub fn snapshot(&self) -> RunMetrics {
        RunMetrics {
            schema_version: RUN_METRICS_SCHEMA_VERSION,
            harness_mode: self.harness_mode.clone(),
            model: self.model.clone(),
            provider: self.provider.clone(),
            wall_clock_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            turns: self.turns,
            tokens: self.tokens.clone(),
            cost: self.cost.clone(),
            tools: self.tools.clone(),
            compaction: self.compaction.clone(),
            tool_repair: self.tool_repair.clone(),
            recovery: self.recovery.clone(),
            prompt: self.prompt.clone(),
            context: self.context.clone(),
            edit_guard: self.edit_guard.clone(),
        }
    }

    /// Snapshot into the serializable artifact at end of run.
    #[must_use]
    pub fn finish(self) -> RunMetrics {
        self.snapshot()
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

    fn assistant_from(provider: &str, usage: Usage) -> AgentEvent {
        assistant_stopped(provider, usage, StopReason::Stop)
    }

    fn assistant_stopped(provider: &str, usage: Usage, stop_reason: StopReason) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::Text { text: "x".into() }],
                usage,
                stop_reason,
                error_message: None,
                provider: provider.into(),
                model: "m".into(),
                timestamp: 1,
                reasoning_details: None,
            }),
        }
    }

    fn rlm_stats(
        evicted_count: u64,
        evicted_tokens: u64,
        paged_in_count: u64,
        paged_in_tokens: u64,
        ledger_tokens: u64,
        sent_context_tokens: u64,
    ) -> AgentEvent {
        AgentEvent::RlmStatsUpdate {
            archived_messages: 0,
            evicted_count,
            evicted_tokens,
            paged_in_count,
            paged_in_tokens,
            ledger_tokens,
            sent_context_tokens,
        }
    }

    fn guard_classified(verdict: bool) -> AgentEvent {
        AgentEvent::EditGuard {
            signal: EditGuardSignal::Classified(verdict),
        }
    }

    fn guard_fired(round: u32) -> AgentEvent {
        AgentEvent::EditGuard {
            signal: EditGuardSignal::Fired { round },
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

    /// Like [`fold`], with the effective Ollama window recorded —
    /// the truncation detector is off without one.
    fn fold_with_num_ctx(num_ctx: u64, events: &[AgentEvent]) -> RunMetrics {
        let mut a = acc();
        a.set_ollama_num_ctx(Some(num_ctx));
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

    /// rlm2/PR1: the accumulator folds the rlm policy's per-fire
    /// deltas (off `RlmStatsUpdate`) into the `context` block, and
    /// sums Ollama's per-turn `prompt_eval_count` (`input_tokens`)
    /// into `prefill_tokens_total`.
    #[test]
    fn rlm_stats_deltas_accumulate_into_context_metrics() {
        let m = fold_with_num_ctx(16_000, &[
            rlm_stats(2, 1_500, 1, 600, 80, 9_000),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 9_000,
                    output_tokens: 50,
                    ..Usage::default()
                },
            ),
            rlm_stats(1, 700, 0, 0, 90, 9_500),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 9_400,
                    output_tokens: 40,
                    ..Usage::default()
                },
            ),
        ]);
        assert_eq!(m.context.evictions, 3);
        assert_eq!(m.context.evicted_tokens, 2_200);
        assert_eq!(m.context.page_ins, 1);
        assert_eq!(m.context.page_in_tokens, 600);
        assert_eq!(m.context.ledger_tokens_total, 170);
        // Both turns prefilled close to their estimate — no suspicion.
        assert_eq!(m.context.prefill_tokens_total, 18_400);
        assert_eq!(m.context.truncation_suspected, 0);
    }

    /// rlm2/PR1: a turn carrying the context-shift signature —
    /// sent estimate above `num_ctx`, prefill near the window yet
    /// under 0.9× the estimate — is flagged as a suspected silent
    /// truncation (the P1 context-shift bug). A turn that prefills
    /// near-estimate is not.
    #[test]
    fn truncation_suspected_increments_on_context_shift_signature() {
        let m = fold_with_num_ctx(12_000, &[
            // Sent ~16k into a 12k window; Ollama re-evaluated the
            // shifted window (~8k of it) → context-shift.
            rlm_stats(0, 0, 0, 0, 100, 16_000),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 8_000,
                    output_tokens: 30,
                    ..Usage::default()
                },
            ),
            // Sent ~16k, prefilled ~15.5k (within the heuristic band) → fine.
            rlm_stats(0, 0, 0, 0, 100, 16_000),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 15_500,
                    output_tokens: 30,
                    ..Usage::default()
                },
            ),
        ]);
        assert_eq!(m.context.truncation_suspected, 1);
    }

    /// Regression (rlm2 review): Ollama's `prompt_eval_count`
    /// counts only newly-evaluated tokens, so on a healthy
    /// append-only turn the prefix cache covers everything but the
    /// new suffix and the count comes in tiny. That undershoot is a
    /// cache HIT, not a truncation — flagging it would invert the
    /// alarm and fire on every well-behaved multi-turn rlm run.
    #[test]
    fn prefix_cache_hit_prefill_is_not_truncation() {
        let m = fold_with_num_ctx(12_000, &[
            // Send fits the window: no shift is possible, however
            // small the prefill.
            rlm_stats(0, 0, 0, 0, 100, 9_000),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 300,
                    ..Usage::default()
                },
            ),
            // Even over the window, a prefill far below it is the
            // suffix-only cache-hit shape, not a shifted re-eval.
            rlm_stats(0, 0, 0, 0, 100, 16_000),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 300,
                    ..Usage::default()
                },
            ),
            // A fully-cached prompt omits `prompt_eval_count`
            // entirely (input_tokens = 0).
            rlm_stats(0, 0, 0, 0, 100, 16_000),
            assistant_from("ollama", Usage::default()),
        ]);
        assert_eq!(m.context.truncation_suspected, 0);
    }

    /// Without a known `num_ctx` (hosted runs, compat APIs) a
    /// prefill undershoot can't be told apart from a cache hit, so
    /// the detector stays off entirely.
    #[test]
    fn truncation_detector_is_off_without_a_known_num_ctx() {
        let m = fold(&[
            rlm_stats(0, 0, 0, 0, 100, 16_000),
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 8_000,
                    ..Usage::default()
                },
            ),
        ]);
        assert_eq!(m.context.truncation_suspected, 0);
    }

    /// Regression (rlm2 review): the agent loop synthesizes
    /// assistant messages for stream failures and aborts with the
    /// real provider string — those never evaluated the send, so
    /// they must not consume the estimate as a truncation sample.
    #[test]
    fn errored_or_aborted_replies_are_not_prefill_samples() {
        for stop_reason in [StopReason::Error, StopReason::Aborted] {
            let m = fold_with_num_ctx(12_000, &[
                rlm_stats(0, 0, 0, 0, 100, 16_000),
                // Shift-shaped usage on an error-shaped reply.
                assistant_stopped(
                    "ollama",
                    Usage {
                        input_tokens: 8_000,
                        ..Usage::default()
                    },
                    stop_reason,
                ),
            ]);
            assert_eq!(
                m.context.truncation_suspected, 0,
                "{stop_reason:?} replies are not prefill samples"
            );
        }
    }

    /// Unit coverage of the shared predicate: only the full
    /// context-shift signature flags.
    #[test]
    fn prefill_predicate_requires_the_full_shift_signature() {
        // The shift signature: sent over the window, prefill near it.
        assert!(prefill_indicates_truncation(16_000, 8_000, Some(12_000)));
        // Near-estimate prefill: healthy full prefill.
        assert!(!prefill_indicates_truncation(16_000, 15_500, Some(12_000)));
        // Prefill far below the window: cache hit.
        assert!(!prefill_indicates_truncation(16_000, 300, Some(12_000)));
        // Send fits the window: no shift possible.
        assert!(!prefill_indicates_truncation(9_000, 300, Some(12_000)));
        // Omitted prompt_eval_count (fully cached prompt).
        assert!(!prefill_indicates_truncation(16_000, 0, Some(12_000)));
        // Unknown window: detector off.
        assert!(!prefill_indicates_truncation(16_000, 8_000, None));
    }

    /// rlm2/PR1: the detector must never fire on a hosted provider —
    /// `prompt_eval_count` semantics don't apply there, so a low
    /// `input_tokens` relative to our estimate is not a truncation.
    /// The prefill total stays Ollama-only too.
    #[test]
    fn truncation_never_suspected_for_non_ollama_provider() {
        let m = fold_with_num_ctx(12_000, &[
            rlm_stats(0, 0, 0, 0, 100, 16_000),
            // Shift-shaped numbers, hosted provider → never flags.
            assistant_from(
                "openai",
                Usage {
                    input_tokens: 8_000,
                    output_tokens: 30,
                    ..Usage::default()
                },
            ),
        ]);
        assert_eq!(m.context.truncation_suspected, 0);
        assert_eq!(m.context.prefill_tokens_total, 0);
    }

    /// rlm2/PR1: a `RlmStatsUpdate` with no following assistant turn
    /// (or a stale estimate) can't leak into a later turn's check —
    /// the estimate is consumed (`take`) on the first Ollama turn.
    #[test]
    fn truncation_estimate_does_not_carry_across_turns() {
        let m = fold_with_num_ctx(12_000, &[
            rlm_stats(0, 0, 0, 0, 0, 16_000),
            // First Ollama turn consumes the estimate (shift signature).
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 8_000,
                    ..Usage::default()
                },
            ),
            // Second turn has no fresh estimate — must not re-fire.
            assistant_from(
                "ollama",
                Usage {
                    input_tokens: 8_000,
                    ..Usage::default()
                },
            ),
        ]);
        assert_eq!(m.context.truncation_suspected, 1);
    }

    /// Forward-compat: a schema-v4 artifact (no `context` block)
    /// loads with the block defaulted, not an error.
    #[test]
    fn v4_metrics_artifact_loads_with_context_block_defaulted() {
        let v4 = serde_json::json!({
            "schema_version": 4,
            "harness_mode": "rlm",
            "model": "m",
            "provider": "ollama",
            "wall_clock_ms": 5,
            "turns": 1,
            "tokens": TokenMetrics::default(),
            "cost": Cost::default(),
            "tools": ToolMetrics::default(),
            "compaction": CompactionMetrics::default(),
            "tool_repair": ToolRepairMetrics::default(),
            "recovery": RecoveryMetrics::default(),
            "prompt": PromptMetrics::default(),
        });
        let back: RunMetrics = serde_json::from_value(v4).expect("v4 artifact loads");
        assert_eq!(back.context, ContextMetrics::default());
    }

    /// PR 3 (edit_completion_guard): the accumulator folds the
    /// classifier verdict and each guard fire, and counts a
    /// successful mutating-tool end that lands AFTER the fire as an
    /// `edit_after_guard` — the signal the intervention worked.
    #[test]
    fn edit_guard_metrics_fold_classifier_verdict_fires_and_post_fire_edits() {
        let m = fold(&[
            // Classifier ran and said the run is edit-expected.
            guard_classified(true),
            // The guard engaged once.
            guard_fired(1),
            // A real edit lands on the re-engaged turn.
            tool_start("c1", "edit"),
            tool_end("c1", false),
        ]);
        assert_eq!(m.edit_guard.classified_expected, Some(true));
        assert_eq!(m.edit_guard.guard_fired, 1);
        assert_eq!(m.edit_guard.guard_rounds, 1);
        assert_eq!(m.edit_guard.edit_after_guard, 1);
    }

    /// A mutating tool that succeeds BEFORE any fire is not
    /// guard-attributable, and a non-mutating or failed tool after a
    /// fire never counts toward `edit_after_guard`.
    #[test]
    fn edit_after_guard_counts_only_successful_mutations_following_a_fire() {
        let m = fold(&[
            // Pre-fire edit: not attributable to the guard.
            tool_start("c0", "edit"),
            tool_end("c0", false),
            guard_fired(1),
            // Post-fire but non-mutating: ignored.
            tool_start("c1", "grep"),
            tool_end("c1", false),
            // Post-fire mutating but failed: ignored.
            tool_start("c2", "write"),
            tool_end("c2", true),
            // Post-fire successful mutation: counts.
            tool_start("c3", "apply_patch"),
            tool_end("c3", false),
        ]);
        assert_eq!(m.edit_guard.guard_fired, 1);
        assert_eq!(
            m.edit_guard.edit_after_guard, 1,
            "only the post-fire successful mutation counts",
        );
    }

    /// When the classifier never runs (explicit override path emits
    /// no `Classified` event), `classified_expected` stays `None`
    /// and is omitted from the serialized artifact — `None` is
    /// "classifier skipped", meaningfully distinct from a recorded
    /// `Some(false)`.
    #[test]
    fn classified_expected_is_none_and_omitted_when_classifier_did_not_run() {
        let m = fold(&[guard_fired(1)]);
        assert_eq!(m.edit_guard.classified_expected, None);
        let json = serde_json::to_value(&m).expect("serialize");
        assert!(
            json["edit_guard"].get("classified_expected").is_none(),
            "a skipped classifier omits the field rather than emitting null",
        );
    }

    /// Forward-compat: a schema-v5 artifact (no `edit_guard` block)
    /// loads with the block defaulted, not an error.
    #[test]
    fn v5_metrics_artifact_loads_with_edit_guard_block_defaulted() {
        let v5 = serde_json::json!({
            "schema_version": 5,
            "harness_mode": "rlm",
            "model": "m",
            "provider": "ollama",
            "wall_clock_ms": 5,
            "turns": 1,
            "tokens": TokenMetrics::default(),
            "cost": Cost::default(),
            "tools": ToolMetrics::default(),
            "compaction": CompactionMetrics::default(),
            "tool_repair": ToolRepairMetrics::default(),
            "recovery": RecoveryMetrics::default(),
            "prompt": PromptMetrics::default(),
            "context": ContextMetrics::default(),
        });
        let back: RunMetrics = serde_json::from_value(v5).expect("v5 artifact loads");
        assert_eq!(back.edit_guard, EditGuardMetrics::default());
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
