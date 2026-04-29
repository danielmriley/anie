# Plan 03 — RLM recurse intent (shape 2)

**Branch:** TBD.
**Status:** deferred. Ship Plan 02 + measure first; this is
the architectural follow-on that becomes worth doing only
after shape 1 proves the recursion paradigm carries weight
on real tasks.

## Rationale

Shape 1 (Plan 02) puts recursion behind a tool that the
model invokes. The harness *cooperates* but doesn't *own*
the recursion: the model decides when to recurse, what
scope to ask for, and how to interpret the result. That's
fine for adoption, but it has costs:

- The model has to learn the `recurse` tool from prompt
  engineering. Local 8B-class models often need several
  examples before they start reaching for it.
- Recursion budget and depth are enforced at tool-execute
  time (with errors), not at decide time. The model finds
  out it's exhausted by trying.
- Sub-call observability is opaque from the master's REPL
  perspective — we can't reason about "this run made 3
  sub-calls totaling X tokens" cleanly without bolt-on
  telemetry.
- Composition with other intents (verifier, critic, TDD
  cycle) is awkward when recursion is a tool rather than a
  step kind.

Shape 2 promotes recursion from a tool to an `AgentIntent`.
The driver (`AgentRunMachine`) decides when to recurse, the
sub-call is a structurally-distinct iteration, and the
budget/depth are first-class loop concerns.

## Design

### New intent variant

```rust
enum AgentIntent {
    // ... existing variants ...
    Recurse {
        scope: RecurseScope,
        sub_query: String,
        depth: u8,
    },
}
```

### New observation variant

```rust
enum AgentObservation {
    // ... existing variants ...
    SubCallCompleted {
        scope: RecurseScope,
        sub_result: AgentRunResult,
    },
    SubCallFailed {
        scope: RecurseScope,
        terminal_error: Option<ProviderError>,
    },
}
```

### Decide routes to recursion

The `decide_next_step` function inspects the most recent
assistant message for a structured "I want to recurse" cue.
Two ways to source it:

- **Free-form structured output**: the assistant emits
  something like
  `<recurse>{ "scope": ..., "query": ... }</recurse>`
  in its text, the parser extracts it, Decide returns
  `Continue(AgentIntent::Recurse { ... })`.
- **Constrained tool call**: a `recurse` tool *is* still
  available, but the master REPL intercepts its tool calls
  and routes them through `Recurse` intent instead of
  `ExecuteTools`. This is the recommended path — it lets
  shape 1 and shape 2 share the same model-facing surface
  and the eval-suite tests don't have to change.

### Eval and Print

`Eval(Recurse)` builds the sub-agent and drives it via
`AgentRunMachine` exactly like shape 1's tool does — but
inside the REPL phase, not inside a tool. `Print` folds the
sub-result into the parent state as a synthesized
`ToolResult` (so the master continues to think it called a
tool) and emits a tracing span for observability.

### Why this is structurally better than shape 1

- **Budget at the loop level.** The driver knows about
  recursion intents, can refuse `Continue(Recurse)` when
  budget is zero, and folds that into the standard `Decide`
  → `Finish` path with a clean reason. No "the tool
  returned an error you should interpret" round-trip.
- **Composition with other intents.** A future `VerifyEdit`
  or `Critic` intent can interleave with `Recurse`
  naturally — the driver chooses what's next based on state,
  not on what the model thinks to ask for.
- **Tracing.** The `agent_repl_step` span gets
  `intent="Recurse"` and `recurse_depth=N` and the per-step
  observability story stays clean. With shape 1 the sub-
  call is buried inside a tool execution span, which is
  less useful.

## When this becomes worth doing

After Plan 02 + the eval suite are in place, this work pays
off if any of these turn out to be true:

- The model uses `recurse` (shape 1) effectively but
  budget/depth errors are getting in the way.
- The eval suite shows `recurse` is consistently helpful
  but the model under-uses it (because the tool surface
  is too foreign for small models to discover).
- We want to compose recursion with verifier/critic
  intents in ways the tool-shaped surface can't express.

If shape 1 is sufficient — the model reaches for `recurse`
naturally, the budget rarely matters, recursion doesn't
need to compose with other steps — shape 2 stays deferred.

## Files (sketch)

- `crates/anie-agent/src/agent_loop.rs` — add `Recurse`
  intent variant + corresponding observation; eval/print/
  decide arms.
- `crates/anie-agent/src/lib.rs` — re-export new variants.
- `crates/anie-cli/src/controller.rs` — wire up the
  intercept logic (route `recurse` tool calls into
  `Recurse` intent instead of `ExecuteTools`).
- New tests in `crates/anie-agent/tests/agent_loop_recurse.rs`.

## Risks

- **Shape 2 without shape 1's data is speculation.** The
  whole rationale assumes recursion-as-tool isn't enough.
  Ship Plan 02, measure, then revisit.
- **Intent explosion.** Each new intent kind adds a row to
  every match in the loop. Keep the count low: `Recurse`
  is justified, but most other ideas should fit as tools or
  policies.

## Exit criteria

To be filled in if/when this plan is unblocked. At minimum:

- All exit criteria from Plan 02 still hold.
- Tracing shows `recurse_depth` field on the appropriate
  span.
- Behavior characterization tests (PR 1 of REPL refactor)
  pass unchanged.

## Deferred (within shape 2)

- Streaming sub-call events to the master's event channel
  (was deferred from Plan 02; lifts to here).
- Adaptive depth — letting the harness allow more depth
  when sub-calls are clearly making progress.
- Per-scope policies (e.g., recurse-into-file uses a
  different system prompt than recurse-into-message-range).
