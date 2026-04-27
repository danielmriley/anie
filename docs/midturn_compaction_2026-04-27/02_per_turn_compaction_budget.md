# 02 — Per-turn compaction budget

## Rationale

Once mid-turn compaction (plan 04) is in place, a single user prompt
can trigger multiple compactions:

1. Pre-prompt: `maybe_auto_compact` fires before the agent loop
   starts.
2. Mid-turn: after a sampling request, the loop notices the threshold
   is breached and compacts inline.
3. Reactive: a sampling request comes back with
   `ProviderError::ContextOverflow`, the retry policy triggers
   `RetryDecision::Compact`, and the controller compacts + retries.

In a degenerate case — small-context model, large tool outputs —
a turn could enter a thrash where each compaction fails to free
enough headroom and the next iteration compacts again. Codex's
implicit assumption (`codex-rs/core/src/codex.rs:6451` comment:
*"compaction works well in getting us way below the token limit"*)
breaks down on 8K-context models.

We need an explicit budget so a compaction storm surfaces as a
clean failure rather than a silent grind.

## Design

### Budget on the controller

A per-user-turn remaining-budget counter, stored as an
`Arc<AtomicU32>` so the controller (which resets it) and the
agent-loop gate from plan 04 (which decrements it from a
spawned task) can share storage cleanly. Decrement-from-max so
the value at any point IS the remaining budget — no subtraction
math at the read site.

```rust
struct InteractiveController {
    // ...
    /// Compactions still allowed in the current user turn. Reset
    /// to `compaction.max_per_turn` at the start of every
    /// `run_prompt`. Decremented at every successful
    /// `CompactionEnd`. Reads use `Ordering::Acquire`; writes
    /// use `Ordering::Release`. Cloned (`Arc::clone`) into the
    /// `ControllerCompactionGate` from plan 04 so mid-turn
    /// compactions decrement the same atomic.
    compactions_remaining_this_turn: Arc<AtomicU32>,
}
```

- Reset to `max_per_turn` in the `run_prompt` entry point, just
  before `maybe_auto_compact`.
- Decremented at every site that emits a successful
  `CompactionEnd` event:
  - **Pre-prompt path** decrements inside `emit_compaction_end`
    (`crates/anie-cli/src/controller.rs:908-923`).
  - **Reactive path** (the `RetryDecision::Compact`-driven
    compact-and-retry) also routes through
    `emit_compaction_end`.
  - **Mid-turn path** (plan 04) constructs and emits its own
    `CompactionStart`/`CompactionEnd` events from the gate
    task. The gate decrements the atomic directly, immediately
    before sending its `CompactionEnd`. That keeps the
    invariant "every successful `CompactionEnd` corresponds to
    exactly one decrement" without forcing the gate to route
    its events through the controller's helper.
- Read-only accessor `compaction_budget_remaining(&self) -> u32`
  used by the reactive path (`retry_policy::decide`) and the
  mid-turn gate's pre-check.

### Budget config

```toml
[compaction]
max_per_turn = 2   # default
```

- Default 2: covers "compact pre-prompt + compact once mid-turn" or
  "compact pre-prompt + reactive overflow retry once" — the two
  realistic two-compaction-in-one-turn shapes.
- Range validation: `1..=8`. Above 8 is almost certainly a sign
  the model can't cope; let it fail loudly.

### Enforcement points

Three call sites need to consult the budget *before* deciding to
compact:

1. **Pre-prompt path** (`maybe_auto_compact`): the budget reset
   happens just before this call, so the very first compaction is
   always allowed. No new check needed.
2. **Mid-turn path** (plan 04): the controller's hook handler must
   check the budget. If exhausted, skip compaction and let the
   agent continue; if the next request overflows, the reactive
   path will surface the error.
3. **Reactive path** (`retry_policy::decide` →
   `RetryDecision::Compact`): the controller-side handler that
   actually performs the compact-and-retry must check the budget.
   If exhausted, treat as `RetryDecision::GiveUp` with a new
   `GiveUpReason::CompactionBudgetExhausted` variant.

### Exhaustion behavior

When the budget is exhausted and a compaction would otherwise have
fired:

- Mid-turn: emit a system message "Compaction budget exhausted for
  this turn; continuing without further compaction. Context may
  exceed the model's window." Continue the loop. The next
  sampling request will likely fail with `ContextOverflow`, and
  `RetryDecision::GiveUp { CompactionBudgetExhausted }` will close
  the turn with a clear error.
- Reactive: emit the system message above, return the underlying
  `ContextOverflow` error to the user verbatim so they can decide
  whether to switch models, increase context, or trim their prompt.

### Why a counter, not a debounce

Time-based throttle (e.g. "no more than one compaction per 5s")
doesn't fit. Compaction itself can take minutes on a slow local
model, so a 5s window is meaningless. A turn-scoped counter cleanly
maps to "the user submitted one prompt; how many times did anie
shrink its context in service of answering it?"

## Files to touch

- `crates/anie-config/src/lib.rs`
  - Add `max_per_turn: u32` to `CompactionConfig` and
    `PartialCompactionConfig` with default 2 and range 1..=8.
- `crates/anie-cli/src/controller.rs`
  - Add `compactions_remaining_this_turn: Arc<AtomicU32>` field
    (see Design section above for the rationale on the atomic +
    decrement-from-max shape).
  - Reset in `run_prompt` via
    `store(max_per_turn, Ordering::Release)`.
  - Decrement once per successful `CompactionEnd`. Pre-prompt and
    reactive paths decrement inside the controller's
    `emit_compaction_end`. The mid-turn gate from plan 04
    decrements directly (it doesn't route through
    `emit_compaction_end`); see the Design section above for
    why both sites carry the responsibility.
  - Add `compaction_budget_remaining(&self) -> u32` accessor
    (synchronous `Acquire` load) used by mid-turn and reactive
    paths.
- `crates/anie-cli/src/retry_policy.rs`
  - Extend `RetryDecision::decide` to take a fourth arg
    `compaction_budget_remaining: u32` (current signature is
    `decide(&self, error, attempt, already_compacted)` per
    `crates/anie-cli/src/retry_policy.rs:85-90`).
  - Add `GiveUpReason::CompactionBudgetExhausted`.
  - Wire through every caller of `decide` to pass the budget.

## Phased PRs

### PR A — Counter + config knob, no enforcement yet

**Change:**

- Add the `Arc<AtomicU32>` field, the reset on each `run_prompt`,
  and the decrement on each `emit_compaction_end`.
- Add the config key with default 2.
- No call sites consult the counter yet — landing this first lets
  04 reuse it.

**Tests:**

- `controller_compaction_budget_resets_to_max_per_turn_on_run_prompt`
- `controller_compaction_budget_decrements_on_compaction_end`
- `compaction_config_loads_max_per_turn`

**Exit criteria:**

- Counter is observable in `/state` summary (optional but useful).
- No behavioral change.

### PR B — Reactive path enforces the budget

**Change:**

- `RetryDecision::decide(&self, error, attempt, already_compacted,
  budget_remaining)` returns
  `RetryDecision::GiveUp { CompactionBudgetExhausted }` when
  budget is 0 and the error is `ContextOverflow`.
- Update all call sites of `decide`.
- Format `CompactionBudgetExhausted` with an actionable message:
  *"Context overflow; budget of {N} compactions per turn already
  used. Try a smaller prompt, increase /context-length, or raise
  [compaction] max_per_turn."*

**Tests:**

- `retry_policy_gives_up_when_budget_exhausted`
- `retry_policy_still_compacts_when_budget_remains`
- Existing retry tests (with budget threaded through).

**Exit criteria:**

- A turn that overflows after exhausting the budget surfaces the
  new error message instead of looping or retrying once more.

### PR C — Mid-turn path consults the budget

**Note:** this PR depends on plan 04 landing first. If 04 is
already merged when this lands, it bolts on cleanly; otherwise
defer this PR to after 04.

**Change:**

- The mid-turn signal handler (added in plan 04) checks
  `compaction_budget_remaining > 0` before initiating compaction.
- If exhausted, emit a system message and skip the compaction;
  let the loop continue.

**Tests:**

- `midturn_compaction_skipped_when_budget_exhausted`
- `midturn_compaction_runs_when_budget_remains`

## Test plan

In addition to the per-PR lists, an end-to-end test that injects
two consecutive `ContextOverflow` errors (after a fake compaction
that doesn't actually shrink context — e.g. by mocking the
summarizer to return the original messages):

- First overflow → compact → retry (budget consumed: 1).
- Second overflow → compact → retry (budget consumed: 2).
- Third overflow → `GiveUp { CompactionBudgetExhausted }`.

This catches the "compaction storm" failure mode end-to-end.

## Risks

- **Existing retry tests need updating.** Threading
  `budget_remaining` through `decide` will touch every test that
  constructs a `RetryDecision`. Mostly mechanical.
- **User confusion at the limit.** Surfacing
  `CompactionBudgetExhausted` to a user who has never heard of
  "compaction budget" needs a clear actionable message. The
  proposed wording above flags two real fixes (smaller prompt,
  larger context).

## Exit criteria

- [ ] `[compaction] max_per_turn` defaults to 2 and is honored.
- [ ] Reactive overflow path gives up after exhausting the budget
      rather than retrying indefinitely.
- [ ] Mid-turn path (after plan 04) skips compaction when budget
      is exhausted.
- [ ] Existing tests green; new tests cover budget exhaustion in
      both reactive and mid-turn paths.

## Deferred

- **Per-session budget rollups** — could imagine "no more than 10
  compactions per session before nagging the user." Out of scope
  here; would belong in plan 06 (telemetry).
- **Per-budget cooling-off behavior** (e.g. "after exhausting
  budget once, lower it for the next 3 turns to encourage
  recovery"). Speculative.
