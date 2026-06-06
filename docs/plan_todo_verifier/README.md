# Plan/Todo tool + verifier loop

Initiative #3 from `docs/rival_analysis_2026-06-06/README.md`
(impact 4 / effort 3). Two related capabilities that share one
piece of run-scoped state:

- **(A) A plan/todo tool** (TodoWrite-style): the model maintains
  an ordered task list with `pending | in_progress | done`
  statuses. State is held for the duration of a run and rendered
  in the TUI transcript + status bar.
- **(B) A verifier / self-critique loop**: a `BeforeModelPolicy`
  consumer (the seam already exists, default `Noop`) that, on
  configured conditions, injects a **context-only** critique step
  before the next `ModelTurn`.

The grounding for this plan is the verified gap slice
`docs/rival_analysis_2026-06-06/findings_by_lens.json`
(`lens = "agentic-planning-verify"`): findings **PLAN-1**,
**VERIFY-1**, **STEP-CHECK-1** (confirmed gaps), plus **DECOMP-1**
and **RECURSE-1** which are explicitly **deferred** here.

> **pi reference caveat.** The pi tree is not present on this
> machine (`docs/rival_analysis_2026-06-06/README.md`). Where this
> plan would normally cite a pi file:line for shape, it cannot.
> `docs/anie_vs_pi_comparison.md` is the only pi-shape reference,
> and it has no TodoWrite/verifier entry. The rival baselines in
> the findings (Claude Code TodoWrite, "Codex has Plan/Task
> types") are flagged **SPECULATIVE** in the source JSON; this
> plan treats them as hypotheses, not facts, and sizes the shape
> to the *confirmed gap*, not to an assumed rival surface.

---

## 1. Rationale

**The gap is real and narrow.** Comprehensive code search
(finding PLAN-1) confirms anie has no model-facing planning
surface:

- The tool registry exports exactly 8 core tools
  (`crates/anie-tools/src/lib.rs:15-23`:
  `BashTool, EditTool, FindTool, GrepTool, LsTool, ReadTool,
  RecurseTool, WriteTool`) plus 2 web tools. No `Todo`, `Plan`,
  `Outline`, or `Task` tool.
- `SessionEntry` (`crates/anie-session/src/lib.rs:126`) has only
  `Message | Compaction | ModelChange | ThinkingChange | Label`.
- `AgentEvent` (`crates/anie-protocol/src/events.rs`) has no
  task/checkpoint/completion variant.
- `AgentIntent` (`crates/anie-agent/src/agent_loop.rs:644-658`)
  routes among `ModelTurn | ExecuteTools | AppendFollowUps |
  RunCompactionGate | Finish` — no verify/critique step.

Planning today is implicit in conversation text. The compaction
prompt even asks the model to reconstruct "remaining tasks" from
natural language (finding DECOMP-1) — there is no structured
ledger to read.

**The verifier seam is built but unused** (finding VERIFY-1). The
`BeforeModelPolicy` trait is defined at
`crates/anie-agent/src/agent_loop.rs:342-346`, with a default
`NoopBeforeModelPolicy` (`agent_loop.rs:350-357`). Only one
production consumer exists — `ContextVirtualizationPolicy`
(`crates/anie-cli/src/context_virt.rs:467`), which does context
eviction/archival, **not** outcome validation. The trait's own
doc comment (`agent_loop.rs:336-338`) says "Future hooks
(after-model, on-tool-error, before-tool, after-stream-error) get
their own traits when real consumers materialize." A verifier is
exactly the "real consumer" the seam was built for.

**Why now / why cheap.** Both halves reuse seams that already
exist: the `Tool` trait + per-run registry rebuild for (A), the
`BeforeModelPolicy` install path for (B). No new architecture, no
new dependency, no provider change. This is the hallmark agentic
UX (a visible plan the user can watch the model work through) at
integration cost.

**What this is NOT.** It is not task decomposition into a DAG
(DECOMP-1), not parallel sub-agent fan-out (RECURSE-1), and not a
new `AgentIntent` (the `VerifyEdit`/`TddCycle` intents sketched in
`docs/small_model_capability_ideas_2026-04-29.md` §6-7). Those add
loop-control complexity; this plan stays inside the existing
single-`ModelTurn` flow. See **Deferred**.

---

## 2. Design

### 2.1 Decision: the todo list is a **Tool over controller-owned state**

The brief asks us to decide whether the todo list is a *Tool* or
*controller-owned state surfaced via `AgentEvent`*, justified
against how `recurse.rs` / `external_context.rs` already inject
context. **It is both, in the same arrangement the recurse tool
already uses, and that is the point.**

Evidence for the pattern. `RecurseTool` is a model-facing tool
(`crates/anie-tools/src/recurse.rs:50,74-81`) that holds an
`Arc<dyn ContextProvider>` injected at construction. The *durable
state* lives in the controller — an
`Arc<RwLock<ExternalContext>>` (`crates/anie-cli/src/recurse_provider.rs:67-77`,
constructed at `controller.rs:2020`, handed to
`RecurseTool::new` at `controller.rs:2031`). The tool is the
model's keyhole into controller-owned state; the controller owns
the store so the policy and TUI can read it independently.

We mirror that exactly:

- **`TodoList` is controller-owned, run-scoped state.** The
  controller constructs one `Arc<Mutex<TodoList>>` per run (std
  `Mutex` — the list is tiny and accessed synchronously, unlike
  `ExternalContext` which is `tokio::RwLock` because the `File`
  scope does async I/O; we don't need async here).
- **`TodoWriteTool` is the model's writable surface over it.**
  The model *must* be able to write the list, and a `Tool` is the
  only thing that gives the model a JSON-schema'd, validated,
  registered write surface (`Tool` trait,
  `crates/anie-agent/src/tool.rs:109`; schema validation in
  `ToolRegistry::register`, `tool.rs:171-186`). A pure
  "controller-owned state surfaced via `AgentEvent`" design gives
  the model no way to *author* the list — `AgentEvent` is an
  outbound render channel only (consumed in
  `crates/anie-tui/src/app.rs:809-984`), never an inbound one.
- **The verifier policy reads the same `Arc<Mutex<TodoList>>`.**
  Just as `ContextVirtualizationPolicy` and `RecurseTool` both
  hold the same `Arc<RwLock<ExternalContext>>`, the verifier and
  the tool share the same `Arc<Mutex<TodoList>>`. One source of
  truth; everything else reads from it.

So: **Tool for the write path, controller-owned `Arc<Mutex<_>>`
for the state, `AgentEvent`/`StatusUpdate` for the render path.**
A pure-tool design (state owned inside the tool) would hide the
list from the policy and TUI; a pure-event design would make it
unwritable. The recurse precedent already resolved this tension —
we follow it.

### 2.2 `TodoList` shape (new, `crates/anie-tools/src/todo.rs`)

Keep the shape minimal — match the confirmed finding (an ordered
list with three statuses), not a speculative DAG.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, Default)]
pub struct TodoList {
    items: Vec<TodoItem>,
}
```

`TodoList` exposes `replace(Vec<TodoItem>)`, `items()`, and
`counts() -> (done: usize, total: usize)`. No `id`, no
`dependencies`, no `subtasks` — those are DECOMP-1 territory and
deliberately absent (matching the "small shapes are how the
project stays extensible" rule in `CLAUDE.md`).

**Write semantics: full replace, not incremental patch.** The
model sends the entire list every call (Claude Code's TodoWrite
contract). This sidesteps id-tracking and partial-update merge
bugs — the list is small and re-sending it is cheap. Documented
as a deliberate deviation candidate if a future rival shape
demands patch semantics.

### 2.3 `TodoWriteTool` (new, `crates/anie-tools/src/todo.rs`)

```rust
pub struct TodoWriteTool {
    list: Arc<Mutex<TodoList>>,
}
```

- `definition()` returns a `ToolDef`
  (`crates/anie-protocol/src/tools.rs:7`: `name`, `description`,
  `parameters`) named `todo_write`, schema = `{ todos: [{content,
  status}] }`, all required, `status` enum
  `["pending","in_progress","done"]`.
- `execute()` validates args (registry already pre-compiles the
  JSON-schema validator, `tool.rs:171-186`), `replace`s the
  shared list, and returns a `ToolResult` whose text is a compact
  rendering (e.g. `[x] done item / [~] in-progress / [ ]
  pending`). That ToolResult is what shows in the transcript —
  exactly how TodoWrite renders as a tool block in Claude Code,
  and it requires **zero** new `AgentEvent` plumbing (tools
  already surface via `ToolExecStart`/`ToolExecEnd`,
  `events.rs`, rendered at `app.rs:854-884`).
- Errors use the existing typed taxonomy
  (`ToolError::ExecutionFailed`, `tool.rs:126-133`) — no
  string-matching. A malformed status or empty list →
  `ExecutionFailed` with a model-actionable message.

### 2.4 Controller wiring + gate

The controller owns the `Arc<Mutex<TodoList>>` and registers
`TodoWriteTool` into the **per-run** registry, reusing the exact
rebuild path the recurse tool uses
(`crates/anie-cli/src/controller.rs:1789-1798` clones the
bootstrap registry then registers per-run tools on top).

Gate: extend `HarnessMode` with `installs_todo_tool()` /
`installs_verifier()` — mirroring `installs_rlm_features()`
(`crates/anie-cli/src/harness_mode.rs:71`) — OR gate on an env
var. **Decision: gate the tool ON by default for all modes**
(it is harmless context the model can ignore) and gate the
**verifier** OFF by default behind `ANIE_VERIFIER=1`, matching the
noop-by-default discipline of the rlm settings
(`controller.rs:1879-1893`, where `ANIE_ACTIVE_CEILING_TOKENS`
defaults to `u64::MAX` = noop fast path). This keeps the verifier
a pure opt-in with zero behavioral change when unset.

### 2.5 TUI rendering

- **Transcript:** free — the `todo_write` ToolResult renders as a
  tool block via the existing `ToolExecEnd` path
  (`app.rs:873-884`).
- **Status bar:** add `todo_done: u64` / `todo_total: u64` to the
  `AgentEvent::StatusUpdate` struct (`events.rs`, emitted at
  `controller.rs:1697-1709`). The controller reads
  `list.lock().counts()` in `status_event()` — the same place it
  reads `rlm_archived_messages.load(...)` today
  (`controller.rs:1706-1708`). Renders as a `todo: 2/5` segment.
  `StatusUpdate` only fires at user-action boundaries, so mid-run
  count changes refresh at the next boundary; a dedicated
  mid-run `TodoStatsUpdate` event (the `RlmStatsUpdate` analogue,
  `events.rs`) is **deferred** — the transcript ToolResult
  already gives live mid-run feedback.

### 2.6 Verifier policy (new, `crates/anie-cli/src/verifier.rs`)

A `BeforeModelPolicy` consumer installed via
`AgentLoopConfig::with_before_model_policy`
(`agent_loop.rs:415`, install site `controller.rs:1816-1818`).

```rust
pub struct VerifierPolicy {
    list: Arc<Mutex<TodoList>>,
    fired: AtomicBool,        // one-shot latch per run
    enabled: bool,            // ANIE_VERIFIER gate
}
```

`before_model(req: BeforeModelRequest)` (signature at
`agent_loop.rs:345`, fields `context`, `generated_messages`,
`model`, `step_index` at `agent_loop.rs:279-294`) returns:

- `BeforeModelResponse::Continue` when disabled, already fired, or
  the trigger condition is unmet — the noop path, identical to
  `NoopBeforeModelPolicy`.
- `BeforeModelResponse::AppendMessages(vec![critique])` exactly
  once, when the trigger fires. The injected message is a single
  user-role context note ("Before continuing: review your todo
  list against what you have actually observed. For each `done`
  item, confirm you verified it — ran the test, read the file.
  If you cannot verify a claim, mark it `in_progress` and say
  so."). `AppendMessages` is documented as **context-only**
  (`agent_loop.rs:305-310` + call site `agent_loop.rs:916-918`:
  policy messages are *not* added to `generated_messages` and the
  controller does not persist them) — precisely the "context-only
  critique step" the scope asks for.

**Trigger (kept tight, deterministic).** Fire once when the todo
list has `total > 0` and every item is `done` — i.e. the model
believes it is finished. Reading the shared list is what lets the
policy gate on *observed plan state* rather than guessing.
Fallback trigger if no todo tool was used: a step-cadence latch
(`step_index >= ANIE_VERIFIER_MIN_STEP`, default e.g. 4) so the
critique still fires on plan-less runs. Both are one-shot via the
`fired` latch to avoid an infinite critique loop.

**Composition with `ContextVirtualizationPolicy`.** The seam holds
a **single** `Arc<dyn BeforeModelPolicy>` (`agent_loop.rs:381`),
so in rlm mode where context-virt is already installed we cannot
just add a second policy. Introduce a small
`ChainedBeforeModelPolicy` in `anie-agent` (next to
`NoopBeforeModelPolicy`, `agent_loop.rs:350`) that folds a
`Vec<Arc<dyn BeforeModelPolicy>>` over a running context:
`Continue` = no-op, `AppendMessages` = extend, `ReplaceMessages` =
reset; final response is `ReplaceMessages(final)` if anything
changed else `Continue`. Run order: context-virt (replace) first,
verifier (append) second. This is the clean, deterministic way to
have N consumers on a single-Arc seam.

### 2.7 Honest limitation: no before-*finish* hook

`BeforeModelPolicy` fires before each `ModelTurn`
(`agent_loop.rs:909-922`). When the model emits no tool calls the
loop decides `Finish` (`AgentIntent::Finish`, `agent_loop.rs:657`)
and there is **no** `ModelTurn` left for the policy to intercept —
the agent_loop comment confirms after-model hooks do not yet exist
(`agent_loop.rs:336-338`). So the verifier nudges *mid-work* (when
the loop will take another `ModelTurn` after tools), not at the
instant of finalization. True before-finish verification needs an
`AfterModelPolicy` / before-finish hook — **deferred** (§8),
flagged in Risks.

### 2.8 Persistence

MVP keeps `TodoList` **in-memory, run-scoped** — matching the
scope statement "state is held for the run." Nothing lands on a
persisted type, so **`CURRENT_SESSION_SCHEMA_VERSION` stays at 4**
(`anie-session/src/lib.rs:90`). Persisting the plan across
session resume would require a `SessionEntry::Todo` variant + a
schema bump to v5 + a forward-compat test — **deferred** (§8).

### 2.9 Dependencies

No new crate. `TodoList`/`TodoItem` are plain `serde` structs
(serde already pervasive); `TodoWriteTool` uses `async_trait`,
`tokio`, `serde_json` — all already in `anie-tools`. `VerifierPolicy`
uses `async_trait` + `std::sync` — already in `anie-cli`.
**Step:** confirm with `cargo tree -p anie-tools` /
`cargo tree -p anie-cli` before writing code (per `CLAUDE.md`
"reuse existing deps").

---

## 3. Files to touch

| File | Change |
|---|---|
| `crates/anie-tools/src/todo.rs` | **new** — `TodoList`, `TodoItem`, `TodoStatus`, `TodoWriteTool`. |
| `crates/anie-tools/src/lib.rs` | export `TodoWriteTool`, `TodoList`, `TodoStatus` (+ `mod todo;`). |
| `crates/anie-tools/src/tests.rs` | tool unit tests. |
| `crates/anie-cli/src/controller.rs` | own `Arc<Mutex<TodoList>>`; register tool per-run; install/compose verifier; status counts. |
| `crates/anie-cli/src/harness_mode.rs` | `installs_verifier()` gate helper (+ unit test). |
| `crates/anie-cli/src/verifier.rs` | **new** — `VerifierPolicy` (`BeforeModelPolicy` impl). |
| `crates/anie-agent/src/agent_loop.rs` | `ChainedBeforeModelPolicy`. |
| `crates/anie-protocol/src/events.rs` | add `todo_done` / `todo_total` to `StatusUpdate`. |
| `crates/anie-tui/src/app.rs` | render `todo: d/t` status segment. |
| `crates/anie-integration-tests/tests/` | end-to-end tool + verifier-injection test. |
| `docs/arch/anie-rs_architecture.md` | document the todo state + verifier seam (exit criterion). |
| `docs/ROADMAP.md` | mark initiative #3 landed (exit criterion). |

---

## 4. Phased PRs

One commit each, ≤5 files, tests + clippy + fmt green before the
next. Commit style `<area>/<PR#>: <imperative>` with a why-body
and the `Co-Authored-By` line (per `CLAUDE.md`).

### PR 1 — `todo/1: add TodoList model + TodoWriteTool`
Pure `anie-tools` addition, no wiring.
- Files: `anie-tools/src/todo.rs` (new), `anie-tools/src/lib.rs`,
  `anie-tools/src/tests.rs`.
- Tests: `todo_write_replaces_list_wholesale`,
  `todo_write_rejects_unknown_status_with_execution_failed`,
  `todo_write_empty_list_is_execution_failed`,
  `todo_write_result_renders_status_markers`,
  `todo_counts_reports_done_over_total`.
- Exit: tool compiles, schema validates under
  `ToolRegistry::register`, unit tests green; tool not yet
  reachable by any run.

### PR 2 — `todo/2: register todo tool + own run-scoped TodoList in controller`
- Files: `anie-cli/src/controller.rs`,
  `anie-cli/src/harness_mode.rs` (gate helper only if used),
  `anie-cli/src/bootstrap.rs` (if registry helper needs a hook).
- Wire `Arc<Mutex<TodoList>>` constructed per run; register
  `TodoWriteTool` on the per-run registry exactly like recurse
  (`controller.rs:1789-1798`).
- Tests: `todo_tool_registered_for_default_mode`,
  `todo_write_call_mutates_controller_owned_list`.
- Exit: a run can call `todo_write`; the list survives across
  turns within the run; transcript shows the tool result.

### PR 3 — `todo/3: surface todo progress in the status bar`
- Files: `anie-protocol/src/events.rs`,
  `anie-cli/src/controller.rs` (status_event counts),
  `anie-tui/src/app.rs`, `anie-tui/src/tests.rs`.
- Tests: `status_update_carries_todo_done_and_total`,
  `status_segment_renders_todo_progress`,
  `status_segment_absent_when_total_is_zero`.
- Exit: `todo: d/t` segment renders; absent/neutral when no plan.

### PR 4 — `verifier/4: ChainedBeforeModelPolicy + VerifierPolicy (opt-in)`
- Files: `anie-agent/src/agent_loop.rs` (chain),
  `anie-cli/src/verifier.rs` (new),
  `anie-cli/src/controller.rs` (install/compose, env gate),
  `anie-agent/src/agent_loop_policy.rs` (chain tests, test-only).
- Tests: `chained_policy_threads_replace_then_append`,
  `chained_policy_all_continue_returns_continue`,
  `verifier_disabled_returns_continue`,
  `verifier_fires_once_when_all_todos_done`,
  `verifier_injection_is_context_only_not_in_generated_messages`,
  `verifier_step_cadence_fallback_fires_without_todos`.
- Exit: with `ANIE_VERIFIER=1`, a one-shot context-only critique
  is appended on the trigger; with it unset, behavior is
  byte-identical to today (Continue). Composes cleanly under rlm
  mode alongside context-virt.

### PR 5 — `todo/5: end-to-end test + docs`
- Files: `anie-integration-tests/tests/todo_verifier.rs` (new),
  `docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md`.
- Tests: `todo_plan_drives_verifier_critique_end_to_end` (mock
  provider writes a plan, marks all done, asserts the next
  request context contains the critique note and that
  `generated_messages` does not).
- Exit: arch doc + ROADMAP updated; full smoke per
  `docs/smoke_protocol_2026-05-01.md`.

---

## 5. Test plan

Names describe behavior-under-test (per `CLAUDE.md`):

| Test | Where | Guards |
|---|---|---|
| `todo_write_replaces_list_wholesale` | `anie-tools` | full-replace contract, not patch-merge |
| `todo_write_rejects_unknown_status_with_execution_failed` | `anie-tools` | typed `ToolError`, no string-match |
| `todo_write_empty_list_is_execution_failed` | `anie-tools` | empty plan rejected |
| `todo_write_result_renders_status_markers` | `anie-tools` | transcript rendering of `[x]/[~]/[ ]` |
| `todo_counts_reports_done_over_total` | `anie-tools` | status-bar math |
| `todo_tool_registered_for_default_mode` | `anie-cli` | tool reachable in all modes |
| `todo_write_call_mutates_controller_owned_list` | `anie-cli` | one source of truth (shared `Arc<Mutex>`) |
| `status_update_carries_todo_done_and_total` | `anie-protocol`/`anie-cli` | new fields populated |
| `status_segment_renders_todo_progress` | `anie-tui` | `todo: d/t` visible |
| `status_segment_absent_when_total_is_zero` | `anie-tui` | no noise on plan-less runs |
| `chained_policy_threads_replace_then_append` | `anie-agent` | chain fold order (virt then verifier) |
| `chained_policy_all_continue_returns_continue` | `anie-agent` | noop preserved through chain |
| `verifier_disabled_returns_continue` | `anie-cli` | opt-in default-off |
| `verifier_fires_once_when_all_todos_done` | `anie-cli` | one-shot latch on plan-complete |
| `verifier_injection_is_context_only_not_in_generated_messages` | `anie-cli` | context-only contract (`agent_loop.rs:916-918`) |
| `verifier_step_cadence_fallback_fires_without_todos` | `anie-cli` | plan-less fallback trigger |
| `todo_plan_drives_verifier_critique_end_to_end` | `anie-integration-tests` | full path under mock provider |

Per-PR validation gate (`CLAUDE.md` / brief): `cargo test
--workspace`; `cargo clippy --workspace --all-targets -- -D
warnings`; `cargo fmt --check`; manual smoke per
`docs/smoke_protocol_2026-05-01.md`.

---

## 6. Risks

- **Verifier fires too late (no before-finish hook).** §2.7 — the
  policy can only nudge before a `ModelTurn` that is still going
  to happen, not at finalization. *Mitigation:* trigger on
  "all-todos-done" so the nudge lands on the turn where the model
  declares completion-via-tool; *punt* true before-finish to an
  `AfterModelPolicy` (Deferred).
- **Critique loop / runaway injection.** A naive trigger could
  inject every turn. *Mitigation:* `AtomicBool` one-shot latch per
  run; the noop fast path returns `Continue` once fired.
- **Single-Arc seam can only hold one policy.** Installing the
  verifier would displace `ContextVirtualizationPolicy` in rlm
  mode. *Mitigation:* `ChainedBeforeModelPolicy` (PR 4) — explicit
  fold with defined Replace-then-Append ordering.
- **Status-bar staleness mid-run.** `StatusUpdate` only fires at
  user-action boundaries (`controller.rs:1697`). *Mitigation:*
  transcript ToolResult gives live feedback; dedicated
  `TodoStatsUpdate` event deferred.
- **Model ignores the tool.** Small/untuned models may never call
  `todo_write`. *Mitigation:* tool is additive (no regression);
  the verifier's step-cadence fallback still fires on plan-less
  runs. We do **not** force-inject a system instruction in MVP
  (avoids prompt bloat); a one-paragraph system-prompt nudge is a
  cheap follow-up if smoke shows low uptake.
- **Over-engineering past the confirmed gap.** *Mitigation:* shape
  is deliberately a flat ordered list (no ids/deps/subtasks),
  matching PLAN-1 not the speculative DAG in DECOMP-1.

---

## 7. Exit criteria

- [ ] `todo_write` tool registered and callable; full-replace
      semantics; typed `ToolError` on bad input.
- [ ] `TodoList` is controller-owned `Arc<Mutex<_>>`, shared by
      the tool and the verifier (one source of truth).
- [ ] Todo progress renders in transcript (tool result) and
      status bar (`todo: d/t`).
- [ ] `VerifierPolicy` injects a one-shot, **context-only**
      critique under `ANIE_VERIFIER=1`; byte-identical to today
      when unset.
- [ ] `ChainedBeforeModelPolicy` composes verifier + context-virt
      deterministically; rlm mode unaffected.
- [ ] `CURRENT_SESSION_SCHEMA_VERSION` unchanged (no persisted
      field).
- [ ] `cargo test --workspace`, `cargo clippy --workspace
      --all-targets -- -D warnings`, `cargo fmt --check` all green.
- [ ] Manual smoke (`docs/smoke_protocol_2026-05-01.md`): a real
      run writes a plan, the verifier critique fires once, no
      loops, no 400s.
- [ ] `docs/arch/anie-rs_architecture.md` documents the todo state
      + verifier seam.
- [ ] `docs/ROADMAP.md` marks initiative #3 landed.

---

## 8. Deferred

Considered and explicitly **not** done here:

- **Parallel sub-agent fan-out (RECURSE-1).** Recurse is serial by
  design (`recurse.rs:215-223`); spawning N concurrent sub-agents
  is a separate, larger initiative. Out of scope.
- **Deep task decomposition / DAG (DECOMP-1).** No
  `{id, dependencies, subtasks}`, no topological routing. The flat
  list matches the confirmed gap; a DAG is speculative
  (SPECULATIVE rival baseline in the findings).
- **Session persistence of the plan.** Would need a
  `SessionEntry::Todo` variant, a bump to
  `CURRENT_SESSION_SCHEMA_VERSION = 5`
  (`anie-session/src/lib.rs:90`) with
  `#[serde(default, skip_serializing_if)]`, and a forward-compat
  test. Punted; MVP is run-scoped.
- **Before-finish verification / `AfterModelPolicy`.** The seam
  for intercepting the *final* turn doesn't exist
  (`agent_loop.rs:336-338`). A new after-model hook is its own
  plan.
- **New `AgentIntent` variants** (`VerifyEdit`, `TddCycle` from
  `docs/small_model_capability_ideas_2026-04-29.md` §6-7). These
  change loop control flow; this plan stays inside the existing
  single-`ModelTurn` `AppendMessages` seam.
- **Mid-run `TodoStatsUpdate` event** (the `RlmStatsUpdate`
  analogue). Transcript ToolResult covers live feedback; add only
  if smoke shows the status bar feels stale.
- **`[verifier]` / `[todo]` TOML config block.** MVP gates on
  `ANIE_VERIFIER` env (matches the rlm-settings precedent,
  `controller.rs:1879-1893`, zero schema churn). Promote to typed
  config once the trigger heuristics stabilize.

---

## Reference

- Verified gap slice:
  `docs/rival_analysis_2026-06-06/findings_by_lens.json`
  (`agentic-planning-verify`: PLAN-1, VERIFY-1, STEP-CHECK-1,
  DECOMP-1, RECURSE-1).
- Calibration corrections:
  `docs/rival_analysis_2026-06-06/README.md`.
- `BeforeModelPolicy` trait / Noop:
  `crates/anie-agent/src/agent_loop.rs:342-357`.
- `BeforeModelRequest` / `BeforeModelResponse`:
  `crates/anie-agent/src/agent_loop.rs:279-294`, `305-318`.
- before_model call site (context-only proof):
  `crates/anie-agent/src/agent_loop.rs:909-922`.
- Existing policy consumer (eviction, not verify):
  `crates/anie-cli/src/context_virt.rs:467`; install at
  `crates/anie-cli/src/controller.rs:1816-1818`.
- Tool trait / `ToolError` taxonomy:
  `crates/anie-agent/src/tool.rs:109`, `126-133`.
- `ToolDef`: `crates/anie-protocol/src/tools.rs:7`.
- Recurse shared-state precedent:
  `crates/anie-tools/src/recurse.rs:50,74-81`;
  `crates/anie-cli/src/recurse_provider.rs:67-77`;
  construction at `crates/anie-cli/src/controller.rs:2020,2031`.
- Per-run registry rebuild:
  `crates/anie-cli/src/controller.rs:1789-1798`.
- `StatusUpdate` emit + `rlm_archived_messages` precedent:
  `crates/anie-cli/src/controller.rs:1697-1709`.
- `AgentEvent` / TUI consumers:
  `crates/anie-protocol/src/events.rs`;
  `crates/anie-tui/src/app.rs:809-984`.
- `SessionEntry` + schema version:
  `crates/anie-session/src/lib.rs:90,126`.
- env-var config precedent:
  `crates/anie-cli/src/controller.rs:1879-1893`.
- Verifier/critic prior sketch (unimplemented):
  `docs/small_model_capability_ideas_2026-04-29.md` §6-8.
- Plan template gold standards:
  `docs/max_tokens_handling/README.md`,
  `docs/tui_responsiveness/README.md`.
