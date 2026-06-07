# Eval harness + metrics export

A minimal, falsifiable evaluation harness so "rival-grade" stops
being a claim and starts being a number. This plan lands two
separable things:

1. **Structured metrics export** from a live `anie` run —
   token usage, latency, tool success/failure rate, cost, and
   compaction counts — written to a sidecar JSON via a new
   `--metrics-out` flag. Builds directly on the existing
   `Usage` / `CompactionPhase` / `harness_mode` plumbing.
2. **A non-interactive scenario runner** (`anie-evals` crate)
   that runs `anie` against a handful of TOML scenarios under
   the existing `--harness-mode {baseline,current,rlm}`
   profiles, scores pass/fail with automated checks, and emits
   a comparison report.

This is intentionally the *first cut*: a handful of scenarios
and a small metrics schema, not the 30–40-scenario,
LLM-as-judge benchmark suite that `docs/rlm_2026-04-29/07_evaluation_harness.md`
specified. That document is the long-range vision; this plan is
the smallest slice that makes harness-mode deltas measurable.

---

## 1. Rationale

### The gap is verified, not speculative

The rival analysis (`docs/rival_analysis_2026-06-06/`,
lens `eval-observability`) confirmed five findings against the
actual code:

- **EVAL-1 / EVAL-5** — no `anie-evals` crate, no scenario
  runner, no scenario corpus. The workspace has 12 members
  (`Cargo.toml:3-16`), not the 14 Plan 07 implied. Only the
  *foundation* — the `--harness-mode` flag — landed.
- **EVAL-2** — no structured metrics export. The data *exists*
  (`Usage` per `AssistantMessage` at
  `crates/anie-protocol/src/messages.rs:38`; `CompactionEnd`
  with `tokens_before`/`tokens_after`/`phase` at
  `crates/anie-protocol/src/events.rs:104-113`) but there is
  no command, flag, or sink that aggregates a run into a
  queryable artifact.
- **EVAL-3** — tool success/failure is observable per-event
  (`AgentEvent::ToolExecEnd { is_error, .. }` at
  `crates/anie-protocol/src/events.rs:60-64`) but never
  aggregated. "read succeeds 98.5% of the time" is
  unanswerable from a live run.
- **EVAL-4** — `HarnessMode` (`crates/anie-cli/src/harness_mode.rs:29-87`)
  correctly gates `baseline`/`current`/`rlm`, but nothing
  runs the same prompt across all three and reports the delta.

### What already exists — build on it, don't rebuild it

The calibration note in
`docs/rival_analysis_2026-06-06/README.md` is explicit: many
features are already built. For this initiative specifically:

- `HarnessMode` enum + `--harness-mode` flag are done and
  tested (`crates/anie-cli/src/lib.rs:78-79`,
  `crates/anie-cli/src/harness_mode.rs`). We **reuse** it as
  the comparison axis — we do not touch it.
- `CompactionStatsAtomic` (`crates/anie-cli/src/compaction_stats.rs:27-95`)
  is the established pattern for per-phase counters. The
  metrics accumulator mirrors its shape rather than inventing
  a new one.
- One-shot print mode (`crates/anie-cli/src/print_mode.rs`)
  already drives a full agent run non-interactively and
  pattern-matches the exact events we need to aggregate
  (`ToolExecEnd` at `:101`, `CompactionEnd` just below). The
  metrics sink hooks into that existing event loop.

### Why this is the right scope

The `eval-observability` rival baselines for EVAL-2 and EVAL-3
are flagged **SPECULATIVE** in the findings ("Claude Code
*likely* has a structured events sink…"). We treat them as
hypotheses, not facts. So we do **not** build a Datadog/
Prometheus/SQLite fleet pipeline (EVAL-6's vision) — we build
the smallest thing that answers the local, verifiable
question: *does rlm mode change tokens/latency/score versus
current on a fixed scenario?* A sidecar JSON per run plus a
small comparison report is sufficient and matches what the
confirmed gaps actually require.

---

## 2. Design

### 2.1 Metrics schema (`RunMetrics`)

A new module `crates/anie-cli/src/run_metrics.rs`. The shape is
deliberately small — every field is sourced from data that
*already flows through the print-mode event loop*, so nothing
new needs to be threaded out of the agent core. One caveat: the
`cost` field is structurally present on `Usage` but never
populated today (cost population is separate initiative #5), so
`RunMetrics.cost` exports `0.0` until that lands — see 2.4 for
the full rationale.

```rust
/// Schema version for the metrics artifact. Independent of the
/// session schema — this is a sidecar file, not a persisted
/// session type. Starts at 1.
pub const RUN_METRICS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunMetrics {
    pub schema_version: u32,
    pub harness_mode: String,        // HarnessMode::label()
    pub model: String,
    pub provider: String,
    pub wall_clock_ms: u64,
    pub turns: u32,
    pub tokens: TokenMetrics,
    pub cost: anie_protocol::Cost,   // reuse — see 2.4
    pub tools: ToolMetrics,
    pub compaction: CompactionMetrics,
}

// Field names mirror `Usage` (`usage.rs:5-20`) exactly so the
// accumulator is a 1:1 copy with no rename layer. `total_tokens`
// here is the *summed* export total (see 2.2), distinct from
// `Usage.total_tokens: Option<u64>` (the provider-reported total).
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
    /// Per-tool (calls, failures), keyed by tool name. BTreeMap
    /// for stable serialization order (golden-test friendly).
    pub by_tool: std::collections::BTreeMap<String, ToolOutcome>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolOutcome {
    pub calls: u32,
    pub failures: u32,
}

/// Mirrors CompactionStats (compaction_stats.rs:77-95) but is
/// the export-facing, serde-derived twin. We count events in
/// the sink rather than reading the in-process atomic, so the
/// metric is self-contained in the print-mode consumer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CompactionMetrics {
    pub pre_prompt: u32,
    pub mid_turn: u32,
    pub reactive_overflow: u32,
    pub total: u32,
}
```

`cost` is `anie_protocol::Cost` verbatim
(`crates/anie-protocol/src/usage.rs:23-35`) — reused, not
re-declared. **anie-specific note to put in the code:** today
`Usage.cost` is structurally present but never populated
(this is the separate "Cost / token-budget enforcement"
initiative #5 in the shortlist). So `cost.total` will be `0.0`
on most providers in the first cut. We export it anyway so the
field is wired the day cost population lands; the runner must
not treat `cost == 0` as an error.

### 2.2 The accumulator (`RunMetricsAccumulator`)

Aggregation logic lives in `run_metrics.rs` as a small state
machine, kept out of the print I/O code so it is unit-testable
against a synthetic `Vec<AgentEvent>`:

```rust
pub struct RunMetricsAccumulator {
    started: std::time::Instant,
    mode: String, model: String, provider: String,
    // running counters …
}

impl RunMetricsAccumulator {
    pub fn new(mode: &HarnessMode, model: &str, provider: &str) -> Self;
    /// Fold one event into the running totals. No I/O.
    pub fn observe(&mut self, event: &AgentEvent);
    /// Snapshot into the serializable artifact.
    pub fn finish(self) -> RunMetrics;
}
```

`observe` maps existing events onto metrics:

| Event (events.rs) | Metric updated |
|---|---|
| `MessageEnd { Message::Assistant(a) }` | add `a.usage.{input_tokens, output_tokens, cache_read_tokens, cache_write_tokens}` into the same-named `TokenMetrics` fields; `cost += a.usage.cost`; `turns += 1` |
| `ToolExecEnd { tool_name?, is_error, .. }` | `tools.calls += 1`; `+= is_error as failures`; bump `by_tool[name]` |
| `CompactionEnd { phase, .. }` | bump the matching `CompactionMetrics` field + `total` |

**Deviation flagged in code:** `ToolExecEnd`
(`events.rs:60-64`) carries `call_id` + `result` + `is_error`
but **not** `tool_name` — the name is only on `ToolExecStart`
(`events.rs:54-58`). So the accumulator must remember
`call_id -> tool_name` from `ToolExecStart` to attribute
per-tool outcomes. A tiny `HashMap<String, String>` cleared as
calls resolve. (Alternative considered: add `tool_name` to
`ToolExecEnd`. Rejected for the first cut — it touches the
protocol crate and every event consumer; the call-id map is
local and cheap. Noted in Deferred.)

`TokenMetrics.total_tokens` (the summed export total): prefer
`usage.total_tokens` when the provider reports it
(`usage.rs:14-16`), else `input_tokens + output_tokens`. Match
how the rest of the codebase treats the optional total — do not
silently double-count.

### 2.3 Export wiring (`--metrics-out`)

- New CLI flag in `crates/anie-cli/src/lib.rs` (alongside
  `--harness-mode` at `:78-79`):
  ```rust
  /// Write a RunMetrics JSON artifact for this run to PATH.
  /// Print mode only; the eval harness sets this.
  #[arg(long, value_name = "PATH")]
  pub metrics_out: Option<PathBuf>,
  ```
- `crates/anie-cli/src/print_mode.rs`: construct the
  accumulator before `process_print_events`, call
  `accumulator.observe(&event)` inside the existing
  `while let Some(event) = …recv().await` loop (the same loop
  that already matches `ToolExecEnd`/`CompactionEnd`), and
  after the loop, if `cli.metrics_out` is set, write the
  artifact with `anie_config::atomic_write` (the existing
  temp-file + fsync + rename helper — reuse, don't `fs::write`
  raw).

No controller change is required: print mode is already the
full non-interactive event consumer, and the metrics we need
are all present in the event stream it drains. (The in-process
`CompactionStatsAtomic` on `ControllerState`
(`controller.rs:1191`) stays as-is for `/state`; the sink
counts `CompactionEnd` events independently and the two should
agree — a property we assert in a test.)

### 2.4 Scenario format (TOML)

A new workspace member `crates/anie-evals/`. Scenarios are
TOML, single-turn in the first cut (multi-turn is Deferred):

```toml
name = "find_compaction_stats_struct"
family = "repo_navigation"
description = "Agent must locate the per-phase compaction counter type."

# Optional fixture. Exactly one of `dir` or `git_ref`, or omit
# for a no-setup scenario that runs in a fresh temp dir.
[fixture]
dir = "fixtures/anie_snapshot"   # copied into a temp cwd
# git_ref = "abc123"             # alt: `git worktree add` at ref

prompt = "Which struct tracks per-phase compaction counts, and in which file?"

# Automated, deterministic assertions. All must pass for PASS.
[expect]
contains = ["CompactionStatsAtomic", "compaction_stats.rs"]  # final text contains ALL
must_call_tool = "grep"     # at least one call to this tool
min_tool_calls = 1
max_tokens = 60000          # total tokens under cap (efficiency)
max_wall_clock_ms = 180000  # wall-clock under cap
```

Loader → `Scenario` struct, `serde(deny_unknown_fields)` so a
typo'd key fails loud rather than silently no-op'ing. Each
scenario file is SHA256-hashed at load (`sha2`, already a
workspace dep) and the hash goes in the report for
reproducibility.

`Expect` is automated-only. **No LLM-as-judge / rubric in the
first cut** — that is the single biggest over-build risk from
Plan 07 and is explicitly Deferred. Automated checks:
`contains`, `must_call_tool` + `min_tool_calls`, `max_tokens`,
`max_wall_clock_ms`. Each maps to one `CheckResult`
(`Pass | Fail { reason } | NotApplicable`).

### 2.5 The runner

`anie-evals` runs each scenario, for each requested
`--harness-mode`, by invoking the built `anie` binary as a
subprocess (black-box, exactly how a rival eval treats the
agent):

```
anie --print \
     --harness-mode <mode> \
     --model <model> \
     --metrics-out <tmp>/metrics.json \
     -C <fixture-tmp-cwd> \
     "<prompt>"
```

It captures stdout (final assistant text → `contains` /
text checks), reads the `RunMetrics` JSON
(→ `must_call_tool`, `max_tokens`, compaction counts), and the
subprocess wall-clock as a sanity cross-check against
`RunMetrics.wall_clock_ms`. The binary path comes from
`CARGO_BIN_EXE_anie` in tests and a `--anie-bin` flag (default:
search `target/release/anie` then `PATH`) for real runs.

Subprocess (not in-process library linkage) is the right
choice: it keeps `anie-evals` a leaf crate with no dependency
on `anie-cli`'s private controller internals, and it exercises
the real CLI surface — including `--harness-mode` and
`--metrics-out` — end to end.

`git_ref` fixtures use `git worktree add --detach <tmp> <ref>`
and remove the worktree after; `dir` fixtures copy a tree into
a `tempfile::TempDir`. Both keep the repo working tree
untouched.

### 2.6 Report

Per `(scenario, mode)` → `RunResult { pass, checks:
Vec<CheckResult>, metrics: RunMetrics }`. The aggregate
`EvalReport`:

```json
{
  "schema_version": 1,
  "run_id": "2026-06-06T0915Z",
  "harness_commit": "8c2470c",
  "scenarios": [{
    "name": "find_compaction_stats_struct",
    "scenario_sha256": "…",
    "results": [
      { "mode": "current", "pass": true,
        "metrics": { "tokens": { "total_tokens": 41200 }, "wall_clock_ms": 33400,
                     "tools": { "calls": 4, "failures": 0 },
                     "compaction": { "total": 0 } } },
      { "mode": "rlm", "pass": true,
        "metrics": { "tokens": { "total_tokens": 29800 }, "wall_clock_ms": 41100,
                     "tools": { "calls": 5, "failures": 1 },
                     "compaction": { "total": 0 } } }
    ]
  }]
}
```

Plus a Markdown summary with the per-scenario `Δ (rlm vs
current)` line (tokens %, latency %, pass/fail). `harness_commit`
comes from `git rev-parse --short HEAD` at runtime.

### 2.7 No session-schema bump

`RunMetrics` is a **sidecar artifact**, not a field on any
persisted session type. `CURRENT_SESSION_SCHEMA_VERSION`
(`crates/anie-session/src/lib.rs:90`, currently `4`) is **not**
bumped. If a later plan persists tool/metrics data *onto* a
`SessionEntry`, that plan bumps it and adds the forward-compat
test — out of scope here.

### 2.8 Dependencies

`cargo tree -p anie-cli` / workspace check first. Everything
needed is already a workspace dep:

- `toml = "0.8"` (`Cargo.toml:79`) — scenario parse.
- `serde` / `serde_json` (`:67`, `:71`) — schema + report.
- `sha2 = "0.10"` (`:66`) — scenario hashing.
- `clap = "4.5"` (`:32`) — the eval binary args.
- `tempfile = "3"` (`:74`) — fixture sandboxes.
- `anyhow` (`:28`), `thiserror = "2"` (`:75`) — errors.

**No new external dependency.** `git`/`anie` are invoked as
subprocesses, not linked.

---

## 3. Files to touch

PR 1 (metrics export — `anie-cli`):
- `crates/anie-cli/src/run_metrics.rs` — new. Schema +
  accumulator + unit tests.
- `crates/anie-cli/src/lib.rs` — `mod run_metrics;` +
  `--metrics-out` flag.
- `crates/anie-cli/src/print_mode.rs` — thread accumulator
  through the event loop; write artifact on completion.

PR 2 (`anie-evals` crate — types + scenario loader):
- `Cargo.toml` — add `crates/anie-evals` to `members`.
- `crates/anie-evals/Cargo.toml` — new.
- `crates/anie-evals/src/lib.rs` — new. `Scenario`, `Expect`,
  `CheckResult`, `RunResult`, `EvalReport`, `EvalError`
  (thiserror).
- `crates/anie-evals/src/scenario.rs` — new. TOML loader +
  SHA256 + automated-check evaluation against a captured
  transcript/metrics.

PR 3 (runner + eval binary + report):
- `crates/anie-evals/src/runner.rs` — new. Subprocess
  orchestration, fixture setup/teardown.
- `crates/anie-evals/src/report.rs` — new. JSON + Markdown.
- `crates/anie-evals/src/bin/evals.rs` — new. CLI:
  `--scenarios`, `--modes`, `--model`, `--out`, `--anie-bin`.

PR 4 (corpus + comparison + smoke):
- `crates/anie-evals/tests/runner_mock.rs` — new (1 Rust source
  module). Golden integration test driving a mock-provider anie
  run + the multi-mode comparison aggregation.
- `crates/anie-evals/scenarios/*.toml` — 4–6 scenario data files
  (TOML, not source) across 2–3 families.
- `crates/anie-evals/scenarios/fixtures/…` — minimal fixture
  data files, all under one directory.
- `docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md` —
  doc updates (see Exit criteria), not source modules.

"≤5 source files per PR" counts **Rust source modules** (`.rs`),
which every PR here respects (PR 4 touches exactly one,
`runner_mock.rs`). Scenario/fixture TOML are test *data* — added
as a corpus under single directories, not edited as code — and
the two doc files are tracked as exit-criteria steps rather than
source. If a reviewer prefers a hard file cap, PR 4 splits
cleanly into PR 4a (the test + corpus + fixtures) and PR 4b (the
two doc updates); they share no code.

---

## 4. Phased PRs

### PR 1 — `eval/PR1: export per-run metrics via --metrics-out`

Schema + accumulator + flag. Lands standalone value: any
print-mode run can now emit a metrics JSON, independent of the
eval crate.

Tests (`crates/anie-cli/src/run_metrics.rs`):
- `accumulator_sums_token_usage_across_assistant_messages`
- `accumulator_prefers_reported_total_tokens_over_input_plus_output`
- `tool_failure_rate_counts_is_error_ends_and_attributes_by_tool_name`
- `tool_outcome_attribution_uses_call_id_from_exec_start`
- `compaction_metrics_count_each_phase_from_compaction_end_events`
- `compaction_metrics_match_compaction_stats_atomic_on_same_event_stream`
- `run_metrics_json_roundtrips_with_schema_version_1`
- `cost_total_zero_is_exported_not_treated_as_missing`

Exit: above green; `--metrics-out` writes a valid artifact in a
manual print-mode run; clippy/fmt clean.

### PR 2 — `eval/PR2: anie-evals scenario format + automated checks`

The crate skeleton, scenario types, TOML loader, and
check evaluation — no subprocess yet. Checks run against a
synthetic `(final_text, RunMetrics)` pair so they are unit
tested in isolation.

Tests (`crates/anie-evals/src/scenario.rs`):
- `loads_minimal_scenario_with_no_fixture`
- `rejects_unknown_top_level_key` (`deny_unknown_fields`)
- `rejects_fixture_with_both_dir_and_git_ref`
- `contains_check_fails_when_any_required_string_absent`
- `must_call_tool_check_reads_tool_metrics_by_name`
- `max_tokens_check_fails_above_cap_passes_at_cap`
- `scenario_sha256_is_stable_for_identical_bytes`

Exit: above green; `cargo build -p anie-evals` clean; crate is
a leaf (no `anie-cli` dep — `cargo tree -p anie-evals`
confirms).

### PR 3 — `eval/PR3: subprocess runner, eval binary, report`

Wires PR 1 + PR 2 together: run a scenario through the real
`anie` binary under a chosen mode, collect, score, report.

Tests:
- `runner_builds_expected_anie_argv_for_mode_and_metrics_out`
  (`runner.rs` — assert argv without spawning)
- `fixture_dir_is_copied_into_temp_cwd_and_cleaned_up`
- `git_ref_fixture_uses_detached_worktree_and_removes_it`
- `report_json_roundtrips_and_includes_scenario_hash`
  (`report.rs`)
- `markdown_summary_renders_delta_line_for_two_modes`

Exit: above green; `anie-evals --scenarios … --modes current
--model <local>` runs one scenario end to end against a live
local model and writes a report; clippy/fmt clean.

### PR 4 — `eval/PR4: scenario corpus + mode comparison + smoke`

4–6 scenarios (≥2 families: `repo_navigation`, `tool_use`),
minimal fixtures, a `target/release/anie`-free golden test
that drives a **mock-provider** run so the runner is CI-safe
without a model, and the multi-mode comparison aggregation.

Tests (`crates/anie-evals/tests/runner_mock.rs`):
- `mock_run_produces_pass_result_and_populated_metrics`
- `baseline_current_rlm_compared_on_one_scenario_yields_three_results`
- `failing_assertion_marks_run_failed_with_reason`

Exit: corpus committed; mock golden test green in
`cargo test --workspace`; a 3-mode comparison report
(`results/2026-06-06-smoke.json` + `.md`) checked in from a
real local-model run; arch doc + ROADMAP updated.

---

## 5. Test plan

Behavior-named tests, by crate:

`anie-cli` (`run_metrics.rs`):
- `accumulator_sums_token_usage_across_assistant_messages`
- `accumulator_prefers_reported_total_tokens_over_input_plus_output`
- `tool_failure_rate_counts_is_error_ends_and_attributes_by_tool_name`
- `tool_outcome_attribution_uses_call_id_from_exec_start`
- `compaction_metrics_count_each_phase_from_compaction_end_events`
- `compaction_metrics_match_compaction_stats_atomic_on_same_event_stream`
- `run_metrics_json_roundtrips_with_schema_version_1`
- `cost_total_zero_is_exported_not_treated_as_missing`
- `metrics_out_absent_writes_no_artifact` (print_mode)

`anie-evals` (`scenario.rs`):
- `loads_minimal_scenario_with_no_fixture`
- `rejects_unknown_top_level_key`
- `rejects_fixture_with_both_dir_and_git_ref`
- `contains_check_fails_when_any_required_string_absent`
- `must_call_tool_check_reads_tool_metrics_by_name`
- `max_tokens_check_fails_above_cap_passes_at_cap`
- `scenario_sha256_is_stable_for_identical_bytes`

`anie-evals` (`runner.rs` / `report.rs`):
- `runner_builds_expected_anie_argv_for_mode_and_metrics_out`
- `fixture_dir_is_copied_into_temp_cwd_and_cleaned_up`
- `git_ref_fixture_uses_detached_worktree_and_removes_it`
- `report_json_roundtrips_and_includes_scenario_hash`
- `markdown_summary_renders_delta_line_for_two_modes`

`anie-evals` (integration, mock provider):
- `mock_run_produces_pass_result_and_populated_metrics`
- `baseline_current_rlm_compared_on_one_scenario_yields_three_results`
- `failing_assertion_marks_run_failed_with_reason`

Per-PR validation gate (all must pass before the next PR):
`cargo test --workspace`; `cargo clippy --workspace
--all-targets -- -D warnings`; `cargo fmt --check`; manual
smoke per `docs/smoke_protocol_2026-05-01.md`.

---

## 6. Risks

- **`ToolExecEnd` lacks `tool_name`.** Per-tool attribution
  depends on a `call_id -> tool_name` map seeded from
  `ToolExecStart`. If an `Update`/`End` arrives without a
  matching `Start` (shouldn't happen, but defensively),
  attribute it to a `"<unknown>"` bucket rather than dropping
  the count. Covered by
  `tool_outcome_attribution_uses_call_id_from_exec_start`.
- **`cost` is always 0 today.** `Usage.cost` is unpopulated
  until the separate cost-enforcement initiative lands. The
  runner must not gate pass/fail on cost; only `max_tokens` is
  an efficiency check in the first cut. Documented inline and
  tested by `cost_total_zero_is_exported_not_treated_as_missing`.
- **Local-model output variance.** A single run is noisy.
  First cut runs each cell once and reports the raw number;
  multi-run averaging + variance is Deferred. Mitigation:
  keep `contains` assertions tolerant (substring, not exact
  transcript match).
- **Subprocess flakiness / no model configured.** The mock
  golden test (PR 4) is the CI-safe path; live-model runs are
  manual and opt-in. The runner surfaces a typed `EvalError`
  (no string-matching) for spawn failure, non-zero exit,
  missing metrics file, and unparseable metrics.
- **Scenario gaming / overfitting Plan 06 to the corpus.** Out
  of scope to fully solve here; noted so the corpus author
  keeps scenarios behavior-focused. Held-out split is
  Deferred to a later expansion.
- **Speculative rival baselines.** EVAL-2/EVAL-3 rival claims
  are marked SPECULATIVE in the findings; we do not build to
  them (no telemetry sink, no fleet pipeline). Risk of
  under-building is accepted — the confirmed gap is local
  measurability, which this delivers.

---

## 7. Exit criteria

- [x] `--metrics-out PATH` writes a valid `RunMetrics` JSON (schema v1)
      from a print-mode run; absent flag writes nothing (gated). (PR1)
- [x] `RunMetrics` covers token usage, latency, tool success/failure
      (overall + per-tool), cost (now populated by the cost initiative;
      0 for free models, exported either way), and compaction by phase.
- [x] Compaction counts agree with `CompactionStatsAtomic` on the same
      event stream (`compaction_metrics_match_compaction_stats_atomic_*`).
- [x] `anie-evals` crate exists, is a workspace member, and is a leaf
      (no internal `anie-*` dep; cargo tree confirms).
- [x] Scenario TOML loads with `deny_unknown_fields`; the four automated
      checks evaluate to pass/fail. (PR2)
- [x] Runner executes a scenario via the real `anie` binary under a
      chosen `--harness-mode`, scores it, and writes JSON + Markdown. (PR3)
- [x] 5 scenarios across 2 families committed under `scenarios/`;
      `tests/corpus.rs` guards them. (PR4)
- [~] A live 3-mode report under `results/` requires a configured model;
      `results/README.md` documents generating it (operator step, not run
      here — no model/API key available).
- [x] Mock-provider (fake-binary) golden test green in
      `cargo test --workspace`. (PR4)
- [x] `cargo test --workspace`, `cargo clippy --workspace --all-targets
      -- -D warnings`, `cargo fmt --check` all clean.
- [x] `CURRENT_SESSION_SCHEMA_VERSION` unchanged (sidecar artifact).
- [x] `docs/arch/anie-rs_architecture.md` updated (anie-evals crate). (PR4)
- [x] `docs/ROADMAP.md` updated (eval harness shipped). (PR4)
- [x] Deviations (cost note, call-id attribution, sink-counts-events,
      RunMetricsView mirror) called out in code comments.

---

## 8. Deferred

Considered and explicitly **not** in this cut:

- **LLM-as-judge / rubric scoring.** Plan 07's biggest piece.
  Automated checks only for now — judge bias, judge cost, and
  median-of-N noise are real and not worth it until the
  automated corpus shows signal. (Plan 07 §Scoring is the
  future home.)
- **Multi-turn scenarios.** Single prompt per scenario in the
  first cut. The `[[turns]]` array, per-turn expectations, and
  the 30-turn recall scenario are Deferred.
- **Multi-run averaging + variance reporting.** One run per
  cell, raw numbers. Add `--runs N` + mean±stddev once the
  corpus is stable.
- **Cross-model sweep (`--models a,b,c`).** First cut takes a
  single `--model`; loop externally if needed. Frontier-model
  budgeting (Plan 07's model table) is Deferred.
- **Fleet observability / metrics sink** (EVAL-6: Prometheus/
  InfluxDB/BigQuery, cross-session trending, anomaly alerts).
  Speculative rival baseline; not built.
- **Adding `tool_name` to `AgentEvent::ToolExecEnd`.**
  Protocol-wide change; the local call-id map avoids it. Pick
  it up if a second consumer needs per-tool attribution.
- **Persisting tool/metrics data onto `SessionEntry`** (would
  require a `CURRENT_SESSION_SCHEMA_VERSION` bump +
  forward-compat test). Out of scope; sidecar JSON suffices.
- **Held-out scenario split / overfitting guard**, **CI
  integration of the eval suite**, **statistical significance
  testing**, **results dashboard/web UI** — all Plan 07
  deferrals that remain deferred.

---

## Reference

- Findings: `docs/rival_analysis_2026-06-06/findings_by_lens.json`
  (lens `eval-observability`, EVAL-1…EVAL-6) +
  `docs/rival_analysis_2026-06-06/README.md` (calibration).
- Long-range vision: `docs/rlm_2026-04-29/07_evaluation_harness.md`.
- Harness mode: `crates/anie-cli/src/harness_mode.rs:29-87`;
  flag at `crates/anie-cli/src/lib.rs:78-79`.
- Counter pattern to mirror:
  `crates/anie-cli/src/compaction_stats.rs:27-95`.
- Print-mode event consumer (hook point):
  `crates/anie-cli/src/print_mode.rs:54` (`ToolExecEnd` `:101`,
  `CompactionEnd` below).
- Usage / Cost: `crates/anie-protocol/src/usage.rs:1-35`;
  `AssistantMessage.usage` at
  `crates/anie-protocol/src/messages.rs:38`.
- Events: `crates/anie-protocol/src/events.rs` —
  `ToolExecEnd` `:60-64`, `CompactionEnd` `:104-113`,
  `CompactionPhase` `:10-29`.
- Session schema (unchanged):
  `crates/anie-session/src/lib.rs:90` (`= 4`).
- Reuse helpers for the mock golden test:
  `crates/anie-integration-tests/src/helpers.rs:58,76`.
- pi shape: no live pi tree on this machine
  (`docs/rival_analysis_2026-06-06/README.md`); pi has no
  documented eval-harness equivalent in
  `docs/anie_vs_pi_comparison.md`, so this is anie-original
  with no pi file:line to cite.
