# Cost / token-budget enforcement — plan for anie

Status: PROPOSED (not started). This doc is the plan only; no
source, ROADMAP, or arch-doc edits have been made yet — those are
listed as plan steps.

Grounding: `docs/rival_analysis_2026-06-06/` lens
`provider-breadth-cost`, findings `COST-1` and `COST-2`
(`findings_by_lens.json`). Initiative #5 on the ranked shortlist
(impact 4 ÷ effort 3). Provider breadth (`PROVIDER-1`,
`DISCOVERY-1`, `BREADTH-1`) is **out of scope** — see §8.

---

## 1. Rationale

### The gap, verified in code

anie already carries all the *data shapes* for cost accounting and
populates none of them. Three facts, each checked against the tree:

1. **`Cost` is defined and wired into `Usage`, but only ever holds
   zeros.** `crates/anie-protocol/src/usage.rs:23-35` defines
   `Cost { input, output, cache_read, cache_write, total: f64 }`
   with `#[derive(Default)]`. `Usage.cost` (`usage.rs:18-19`) is
   `#[serde(default)]`, so it is *already serialized* into every
   persisted assistant message — today as `"cost":{"input":0.0,…}`.

2. **Per-model pricing exists on `Model` and is never read for a
   calculation.** `Model.cost_per_million: CostPerMillion`
   (`crates/anie-provider/src/model.rs:326`), and `CostPerMillion`
   (`model.rs:7-23`) carries `input/output/cache_read/cache_write`
   f64 rates per million tokens. Catalog entries set real values
   (e.g. Anthropic Sonnet input 3.0 / output 15.0 — finding
   `COST-1` cites `models.rs:39-44`). A workspace grep for
   `model.cost_per_million` outside tests returns *zero* arithmetic
   sites: nothing multiplies tokens by a rate.

3. **`Usage` token counts *are* populated and already flow to the
   UI** — the providers fill `input_tokens` / `output_tokens` (e.g.
   `anthropic.rs` `update_usage`), the agent loop attaches `Usage`
   to each `AssistantMessage`, and the TUI already reads
   `assistant.usage.input_tokens` on `MessageEnd`
   (`crates/anie-tui/src/app.rs:848-850`). The cost half of the
   same struct is simply never computed.

### Why cost is currently never populated

The provider streaming parsers (`anthropic.rs`, `openai/streaming.rs`,
`ollama_chat/streaming.rs`) build `Usage` from the wire token
counts. They do **not** see `Model.cost_per_million` — pricing
lives on the registered `Model`, which the *agent loop* owns
(`AgentLoopConfig.model`), not the provider parser. So no single
existing call site has both halves (token counts *and* the rate
table) in hand at the moment `Usage` is finalized. The result:
`Usage.cost` rides along as `Cost::default()` from creation to
persistence. This plan closes the loop at the one place that *does*
hold both: the agent loop, right after the provider returns the
assistant message.

### Why enforcement matters

`COST-2` confirms there is no `max_cost` / `cost_ceiling` /
`budget_dollar` anywhere in the workspace, no cost field in
`CompactionConfig`/`ContextConfig` (`anie-config/src/lib.rs`), and
no cost branch in `RetryPolicy::decide`. anie can spend unbounded
money on a single runaway turn (tool-loop ping-pong, reasoning
storms) with no guardrail. A per-run / per-session ceiling is the
cheapest high-trust differentiator on the shortlist: the data is
already there, only the multiply + a comparison + a clean stop are
missing.

### Rival baseline — treat as hypothesis, not fact

The findings note pi's `AssistantMessage.usage` reports cost inline
(`pi_summary.md:77`) but explicitly flag the *enforcement* claims as
**SPECULATIVE**: "no evidence found of per-session cost ceiling in
pi's code either." The pi reference tree is **absent from this
machine** (`docs/anie_vs_pi_comparison.md` is the only pi-shape
reference; no live pi `file:line` is citable). So this plan does
**not** assert any rival ships ceiling enforcement. We build the
minimal confirmed-gap scope — compute cost, surface it, add an
*optional* ceiling — and nothing speculative on top.

---

## 2. Design

Three layers, smallest shape that closes the gap.

### 2.1 Compute cost — `CostPerMillion::cost_of`

A pure method on the pricing record, in `anie-provider`
(`crates/anie-provider/src/model.rs`, next to `CostPerMillion`).
`anie-provider` already depends on `anie-protocol`
(`anie-provider/Cargo.toml:24`), so it can see both `Usage` and
`Cost`:

```rust
impl CostPerMillion {
    /// Money for a usage record at these per-million rates.
    /// Cache reads/writes priced separately; `total` is the sum.
    /// Cost is **derived**, never trusted from the wire — anie has
    /// no provider that reports a billed amount today (OpenRouter's
    /// generation-cost endpoint is not queried). Deviation from
    /// pi, which reports provider-billed cost inline.
    #[must_use]
    pub fn cost_of(&self, usage: &Usage) -> Cost {
        let per = |tokens: u64, rate: f64| (tokens as f64) * rate / 1_000_000.0;
        let input = per(usage.input_tokens, self.input);
        let output = per(usage.output_tokens, self.output);
        let cache_read = per(usage.cache_read_tokens, self.cache_read);
        let cache_write = per(usage.cache_write_tokens, self.cache_write);
        Cost { input, output, cache_read, cache_write,
               total: input + output + cache_read + cache_write }
    }
}
```

Note: anie's catalog/discovery cost flows
`ModelInfo.pricing: Option<ModelPricing>` (per-token *string*
decimals from OpenRouter, `model.rs:280-299`) →
`Model.cost_per_million: CostPerMillion` (per-million f64). The
authoritative number for a *registered* model is always
`cost_per_million`; `cost_of` reads that, not `ModelPricing`.

**Population site (single choke point):** the agent loop, right
after the provider hands back the assistant message and before it
emits `MessageEnd`. The loop already owns `self.config.model`. Set
`assistant.usage.cost = self.config.model.cost_per_million.cost_of(&assistant.usage)`
at the assistant-message finalization point near
`agent_loop.rs:835` / `:1222`. Local models (`CostPerMillion::zero()`,
the catalog default — `model.rs:18-22`) yield an all-zero `Cost`,
which is correct (free).

Because `Usage.cost` already serializes and is `#[serde(default)]`,
populating it with real numbers **changes no schema shape** — older
sessions load with `cost` defaulted to zeros, newer sessions write
real values. **No `CURRENT_SESSION_SCHEMA_VERSION` bump.** (Current
value is 4, `anie-session/src/lib.rs:90`.) A forward-compat test
still gets added to lock this invariant (§5).

### 2.2 Surface running cost — `/state` and TUI status

**Accumulation.** A small session-scoped meter mirroring the blessed
`CompactionStatsAtomic` pattern
(`crates/anie-cli/src/compaction_stats.rs`): a new
`crates/anie-cli/src/cost_meter.rs` holding `run` and `session`
`Usage` accumulators plus the model's current `CostPerMillion` so a
running `Cost` can be recomputed on read. Unlike the compaction
counters (`AtomicU32`), cost is `f64` + multi-field, so the meter is
a `Mutex<CostLedger>` rather than lock-free atomics — updates happen
only at turn boundaries (`finish_run` / on `TurnEnd`), never on a
hot path, so the mutex cost is irrelevant. Shape:

```rust
struct CostLedger { run: Usage, session: Usage }  // token sums
// run cost  = pricing.cost_of(&run)
// session cost = pricing.cost_of(&session)
```

- **Run** = one user prompt's agent loop (`start_prompt_run` →
  `finish_run`). Reset at the top of `start_prompt_run`, alongside
  the existing per-turn compaction-budget reset
  (`controller.rs:1016-1018`).
- **Session** = cumulative across runs. Accumulated in `finish_run`
  by summing `assistant.usage` over `result.generated_messages`.
- **`--continue` durability:** session totals are **rebuilt on load**
  by summing `usage` over persisted assistant messages — no new
  persisted field, no schema bump. (`Usage.cost` already persists,
  so even a re-priced backfill is available; we sum token counts and
  re-multiply against the *current* model to stay consistent with the
  live meter.) Document this as an anie-specific choice inline.

**`/state`.** Add a "Cost this session" block to `format_state_summary`
(`controller.rs:2175`), threaded through `state_summary_message`
(`controller.rs:1272`) exactly like the `compaction_stats` snapshot
already is (`controller.rs:1282`). Shows run + session token totals
and dollar cost, and — when a ceiling is configured — the limit and
percent consumed.

**TUI status bar.** The TUI already reads `assistant.usage` on
`MessageEnd` (`app.rs:848`). Extend the same arm to accumulate a
session `Cost` into `StatusBarState` and render a `$0.0123` segment
in `format_status_text` (`app.rs:2298-2311`), consistent with the
existing `mode:` / `archive:` segments. No new `AgentEvent` —
reuse the `MessageEnd` data already on the wire. The segment is
omitted when session cost is exactly 0.0 (local/free models), to
avoid noise.

### 2.3 Enforce optional ceilings — typed terminal condition

**Config (`anie-config`).** A new `BudgetConfig`, small shape, all
fields optional and `None` by default (feature is opt-in, zero
behavior change when unset):

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BudgetConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_run_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_run_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_session_tokens: Option<u64>,
}
```

"cost *and/or* token ceilings" → four optionals (run/session ×
cost/tokens). This lives in `config.toml` `[budget]`, **not** on the
session header — so no session-schema change.

**Two enforcement points, both typed, neither a panic:**

1. **Pre-run gate** (`start_prompt_run`, `controller.rs:989`):
   before appending the prompt and spawning the run, if the *session*
   meter already meets-or-exceeds a session ceiling, refuse to start.
   Emit a `SystemMessage` ("Session budget reached: $X of $Y; raise
   `[budget] max_session_cost_usd` or start a new session with `/new`")
   and return early. Cheap, catches "you already spent it."

2. **In-loop gate** via the already-plumbed `BeforeModelPolicy`
   seam (`agent_loop.rs:342`, the project's designated cross-step
   extension point — calibration notes flag this seam as the place a
   verifier/policy consumer belongs). Stops a *runaway single turn*
   at the next step boundary. Two surgical additions:
   - `BeforeModelRequest` gains `run_usage: &Usage`
     (`agent_loop.rs:279-295`) so a policy can see accumulated
     run cost without holding shared state.
   - `BeforeModelResponse` gains a third terminal variant
     (`agent_loop.rs:307-328`):
     ```rust
     /// Halt the run before the next model request. The loop
     /// finalizes cleanly (no further provider call) and records
     /// `reason` as the run's terminal condition.
     StopRun(RunStopReason),
     ```
     with `RunStopReason::BudgetExceeded { scope: BudgetScope,
     limit: BudgetLimit, spent: f64 }` (a small typed enum, not a
     string). The loop's single match site (`agent_loop.rs:915-921`)
     adds a `StopRun` arm that finalizes the run.

   A new `BudgetPolicy` (`crates/anie-cli/src/budget_policy.rs`)
   implements `BeforeModelPolicy`: pure comparison of
   `run_usage`-derived cost (+ the session baseline captured at run
   start) against the configured ceilings, returning `Continue` or
   `StopRun(BudgetExceeded{…})`. Installed via the existing
   `with_before_model_policy` builder (`agent_loop.rs:415`) in
   `build_agent` (`controller.rs:1807-1818`). The rlm policy is
   unaffected — `BudgetPolicy` and the rlm `ReplaceMessages` policy
   are different impls; only one is installed per run, and rlm's
   impl never returns the new variant, so its exhaustive `match`
   needs no change. (If both are ever needed at once, that's a
   composing-policies follow-up, explicitly deferred §8.)

**Terminal-condition plumbing.** `AgentRunResult`
(`agent_loop.rs:500-509`) currently carries
`terminal_error: Option<ProviderError>`. A budget stop is **not** a
provider failure, so it does **not** become a `ProviderError`
variant (that taxonomy is for wire/stream failures — see its own
doc comment, `error.rs:1-11`). Instead `AgentRunResult` gains a
sibling `terminal_stop: Option<RunStopReason>`. The controller's
`finish_run` (`controller.rs:1567`) inspects it and emits a clean
`SystemMessage`; the partial run's generated messages are still
persisted (we never discard work in flight).

**Why not the `CancellationToken`?** Cancelling
(`controller.rs` cancel path) would surface a budget stop as a
user-abort — untyped, indistinguishable from Ctrl-C. The brief
requires a *typed* terminal condition the controller surfaces
cleanly, so `StopRun(RunStopReason)` is the right seam.

### Deviations called out

- Cost is **derived from token counts × catalog price**, never read
  from a provider's billed amount (pi reports provider-billed cost
  inline; anie has no provider that returns one today). Flagged in
  the `cost_of` doc comment.
- Session cost is **recomputed on load**, not persisted as a
  first-class field — avoids a schema bump; the trade is that a
  mid-session price change in the catalog re-prices history. Noted
  inline at the meter.

---

## 3. Files to touch

PR1 (compute + populate):
- `crates/anie-provider/src/model.rs` — `CostPerMillion::cost_of`.
- `crates/anie-agent/src/agent_loop.rs` — populate `assistant.usage.cost`.
- `crates/anie-protocol/src/usage.rs` — (test-only) forward-compat test.

PR2 (surface):
- `crates/anie-cli/src/cost_meter.rs` — new ledger module.
- `crates/anie-cli/src/controller.rs` — wire meter into
  `start_prompt_run` / `finish_run`, add `/state` block.
- `crates/anie-cli/src/lib.rs` — `mod cost_meter;`.
- `crates/anie-tui/src/app.rs` — accumulate + render session-cost segment.

PR3 (enforce):
- `crates/anie-config/src/lib.rs` — `BudgetConfig` + `AnieConfig.budget`.
- `crates/anie-agent/src/agent_loop.rs` — `BeforeModelRequest.run_usage`,
  `BeforeModelResponse::StopRun`, `RunStopReason`,
  `AgentRunResult.terminal_stop`, loop match arm.
- `crates/anie-cli/src/budget_policy.rs` — new `BudgetPolicy`.
- `crates/anie-cli/src/controller.rs` — pre-run gate, install policy,
  surface `terminal_stop` in `finish_run`.
- `crates/anie-cli/src/lib.rs` — `mod budget_policy;`.

(Each PR ≤ 5 source files. PR3's controller + agent_loop are the
heaviest; budget-policy logic is isolated in its own module to keep
the controller diff small.)

---

## 4. Phased PRs

### PR1 — `cost/PR1: derive and populate Usage.cost from model pricing`

Compute the cost and fill it at the one site that has both halves.
No surfacing, no enforcement yet.

- `CostPerMillion::cost_of(&Usage) -> Cost`.
- Agent loop sets `assistant.usage.cost` before `MessageEnd`.
- Confirm `Usage.cost` already serializes (it does) → no schema bump.

Tests: §5 PR1 list.

Exit:
- `cost_of` returns per-field + summed total; zero pricing → zero cost.
- A run against a priced model yields a non-zero `usage.cost` on the
  persisted assistant message; a local (zero-price) model yields zeros.
- Forward-compat: a v4 session written before this change loads with
  `cost` defaulted, no error.
- `cargo test`/`clippy`/`fmt` green; smoke per §validation.

### PR2 — `cost/PR2: surface running run/session cost in /state and TUI`

Read-only accounting on top of PR1's populated field.

- `cost_meter.rs` ledger; reset-per-run + accumulate-per-session.
- Rebuild session total on load by summing persisted `usage`.
- `/state` "Cost this session" block.
- TUI status `$…` segment (omitted at 0.0).

Tests: §5 PR2 list.

Exit:
- `/state` shows run + session tokens and dollar cost matching the
  sum of turn usages.
- TUI status segment appears for priced models, hidden for free ones.
- `--continue` reopens a session and the meter reflects prior spend.
- Gates green; smoke shows the segment update after a real turn.

### PR3 — `cost/PR3: optional per-run/session cost+token ceilings (typed stop)`

Opt-in enforcement; no behavior change when unconfigured.

- `BudgetConfig` (4 optionals, default None) in `[budget]`.
- `BeforeModelRequest.run_usage`, `BeforeModelResponse::StopRun`,
  `RunStopReason`, `AgentRunResult.terminal_stop`, loop arm.
- `BudgetPolicy` installed in `build_agent`.
- Pre-run session gate in `start_prompt_run`.
- `finish_run` surfaces `terminal_stop` as a `SystemMessage`.

Tests: §5 PR3 list.

Exit:
- Unset config ⇒ byte-identical behavior (Noop-equivalent path).
- Run ceiling stops a runaway turn at the next step boundary with a
  typed `RunStopReason::BudgetExceeded`, not a panic, not a
  `ProviderError`; partial work persists.
- Session ceiling already met ⇒ `start_prompt_run` refuses cleanly.
- Token ceilings enforced symmetrically with cost ceilings.
- `/state` shows limit + percent consumed when a ceiling is set.
- Gates green; smoke: set a tiny `max_run_cost_usd`, observe a clean
  stop message mid-run.

---

## 5. Test plan

Behavior-named tests, each in the crate closest to the logic.

**PR1** (`anie-provider/src/model.rs`, `anie-agent`, `anie-protocol`):
- `cost_of_multiplies_each_token_class_by_its_per_million_rate`
- `cost_of_total_is_sum_of_the_four_token_classes`
- `cost_of_zero_pricing_yields_zero_cost_for_local_models`
- `cost_of_prices_cache_read_and_write_independently`
- `agent_loop_populates_usage_cost_from_model_pricing_before_message_end`
- `agent_loop_leaves_cost_zero_when_model_pricing_is_zero`
- `usage_with_populated_cost_roundtrips_through_session_schema_v4`
  (forward/back-compat: old `cost`-less / zero-`cost` entry loads with
  `Cost::default()`; re-serialize is byte-stable)

**PR2** (`anie-cli/src/cost_meter.rs`, `controller_tests.rs`,
`anie-tui/src/tests.rs`):
- `cost_meter_run_total_resets_at_each_new_prompt`
- `cost_meter_session_total_accumulates_across_runs`
- `cost_meter_rebuilds_session_total_from_persisted_usage_on_continue`
- `state_summary_includes_cost_block_with_run_and_session_dollars`
- `state_summary_cost_block_shows_zero_for_local_model_session`
- `status_bar_renders_session_cost_segment_for_priced_model`
- `status_bar_omits_cost_segment_when_session_cost_is_zero`

**PR3** (`anie-config`, `anie-agent`, `anie-cli/src/budget_policy.rs`,
`controller_tests.rs`):
- `budget_config_defaults_all_ceilings_to_none`
- `budget_config_absent_section_loads_as_all_none`
- `budget_policy_continues_when_no_ceiling_configured`
- `budget_policy_stops_run_when_run_cost_exceeds_ceiling`
- `budget_policy_stops_run_when_run_token_ceiling_exceeded`
- `budget_policy_counts_session_baseline_plus_run_usage_against_session_ceiling`
- `before_model_stop_run_finalizes_loop_without_further_provider_call`
- `agent_run_result_carries_terminal_stop_not_provider_error_on_budget_stop`
- `start_prompt_run_refuses_when_session_cost_ceiling_already_met`
- `finish_run_surfaces_budget_stop_as_system_message_and_persists_partial_work`
- `budget_stop_is_not_classified_as_a_retryable_condition`

---

## 6. Risks

- **`f64` cost drift / display jitter.** Summing per-turn f64 costs
  accumulates rounding. *Mitigation:* keep the ledger in **token
  counts** (`u64`) and multiply once at read time; never sum dollar
  amounts. Format to 4 decimal places for display only.
- **Mid-session price change re-prices history.** Recompute-on-load
  uses the *current* model's rates. *Mitigation:* accept it (documented
  deviation); it only matters if the operator edits the catalog
  mid-session, and the live meter and `/state` stay self-consistent.
- **Seam change touches the rlm policy path.** Adding a
  `BeforeModelResponse` variant forces every loop-side `match` to
  handle it. *Mitigation:* only the loop's single call site
  (`agent_loop.rs:915`) matches the response; the rlm *impl* returns
  its own variants and is untouched. Verified there is exactly one
  match site. A regression test asserts the rlm `ReplaceMessages`
  path still works.
- **Ceiling stops mid-tool-loop, orphaning a tool call.** Stopping at
  a step boundary is *before* the next model request, so no tool
  call is left half-issued; the assistant/tool transcript stays
  balanced. *Mitigation:* gate fires only at the `before_model`
  boundary, never between a tool call and its result.
- **Token estimate vs. billed reality.** Provider token counts are
  authoritative for *their* billing but anie's catalog price may lag
  upstream. *Mitigation:* this is an *estimate* and labeled as such
  in `/state` ("est."); the ceiling is a guardrail, not an invoice.
- **Pre-run gate vs. in-loop gate double-reporting.** Both could fire
  for the same overage. *Mitigation:* pre-run gate only blocks
  *starting* a new prompt; in-loop gate only fires *within* a run;
  they cannot both fire for one prompt.
- **Over-scoping past the confirmed gap.** Rival ceiling enforcement
  is SPECULATIVE. *Mitigation:* ship the four-optional `BudgetConfig`
  and nothing more — no budgets-per-tool, no time windows, no
  multi-key aggregation (§8).

---

## 7. Exit criteria

- [x] `CostPerMillion::cost_of` derives per-field + total cost;
      zero-priced models yield zero. (PR1)
- [x] Agent loop populates `assistant.usage.cost` at message
      finalization; cost flows to session, `/state`, and TUI. (PR1/PR2)
- [x] **No** `CURRENT_SESSION_SCHEMA_VERSION` bump; forward-compat test
      green (`usage_with_populated_cost_roundtrips_through_session_schema_v4`).
- [x] `/state` shows run + session token and dollar totals; TUI status
      shows a session-cost segment for priced models. (PR2)
- [x] `BudgetConfig` is opt-in, defaults all-None, zero behavior change
      when unset. (PR3)
- [x] Ceiling overage is a **typed** `RunStopReason::BudgetExceeded`
      surfaced cleanly — not a panic, not a `ProviderError`; partial work
      persists (proven by `before_model_stop_run_finalizes_loop_*` and
      `agent_run_result_carries_terminal_stop_not_provider_error_*`). (PR3)
- [x] Session-ceiling pre-run gate refuses a new prompt cleanly
      (`session_budget_block` + `format_run_stop` tested). (PR3)
- [x] `cargo test --workspace` green.
- [x] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [x] `cargo fmt --check` clean.
- [~] Manual smoke: covered at the unit level (cost derivation, meter,
      StopRun loop finalization, budget-policy ceilings, surfacing text);
      a live-model drive needs an API key (not run here).
- [x] `docs/arch/anie-rs_architecture.md` updated (cost meter, budget
      policy, the `BeforeModelResponse::StopRun` seam).
- [x] `docs/ROADMAP.md` updated.
- [x] Commit messages follow `cost/PR<n>: <imperative>` + why-body +
      `Co-Authored-By`.

> Scoping note: the heaviest controller-integration tests the plan listed
> (`start_prompt_run_refuses_*`, `finish_run_surfaces_*`) are covered by
> the unit tests of their constituent pure logic (`session_budget_block`'s
> `snapshot ≥ limit` comparison via the cost-meter + budget-policy tests;
> `format_run_stop`'s rendering; the loop's `StopRun` finalization) rather
> than a full spawned-controller harness.

---

## 8. Deferred (considered, explicitly not done)

- **Provider breadth** (`PROVIDER-1`, `DISCOVERY-1`, `BREADTH-1`):
  Gemini/Bedrock providers, more `ApiKind` impls. Out of scope by
  the brief — OpenRouter already offsets breadth.
- **Provider-billed cost ingestion** (OpenRouter generation-cost
  endpoint, Anthropic billing headers). anie derives cost from
  catalog price; querying a real billed amount is a follow-up.
- **`Cost` as a persisted first-class session-total field.** Avoided
  to skip a schema bump; session total is recomputed on load. Revisit
  only if recompute proves too slow on very long sessions.
- **Composing multiple `BeforeModelPolicy` impls** (budget + rlm at
  once). Only one policy installs per run today; a policy-chain
  combinator is a separate refactor.
- **Cancellation-based enforcement.** Rejected: surfaces as an
  untyped abort. Using the typed `StopRun` seam instead.
- **Per-tool / time-window / multi-key budgets, hard *kill* vs. warn
  thresholds, cost telemetry export.** Speculative past the confirmed
  gap; the eval/metrics initiative (#6) owns export.
- **Pricing for cache-tier token classes beyond the four already on
  `CostPerMillion`.** Match the existing shape; extend only when a
  provider reports a fifth class.
