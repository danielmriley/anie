# PR 4 — Step tracing spans

**Goal:** Add structured `tracing` spans at each REPL boundary
so step progression is inspectable in debug logs and tooling
without changing the public `AgentEvent` protocol.

This PR is debug-only. No user-visible behavior change.

## Rationale

After PR 3, the agent loop has well-defined Read / Eval / Print /
Decide phases, but operators have no easy way to see what's
happening at runtime. The architecture doc
(`docs/repl_agent_loop_2026-04-27.md`) calls this out: tracing
spans should be the first observability surface, not new
`AgentEvent` variants — protocol churn would force the TUI and
RPC to change in lockstep with internal refactors.

The current `tracing` use in `agent_loop.rs` is one `warn!` at
line 706 (compaction-gate failure) and one at line ~1381 (tool-
call delta for unknown id). PR 4 adds spans, not warnings.

## Design

### Span hierarchy

```
agent_run                       (root span, one per AgentLoop::run call)
├── agent_repl_step             (one per REPL iteration)
│   ├── agent_read              (Read phase)
│   ├── agent_eval              (Eval phase)
│   │   └── agent_provider_stream  (existing-or-new, scopes collect_stream)
│   │   └── agent_tool_batch       (existing-or-new, scopes execute_tool_calls)
│   ├── agent_print             (Print phase)
│   └── agent_decide            (Decide phase)
```

> Spans `agent_provider_stream` and `agent_tool_batch` are
> *useful* but optional for PR 4. If they fall out naturally as
> `#[instrument]` on `collect_stream` and `execute_tool_calls`,
> add them; otherwise defer to a later observability PR.

### Span fields

```rust
#[instrument(
    skip_all,
    fields(
        run_step = %step_index,
        intent = ?intent.kind(),
        context_messages = state.context.len(),
        generated_messages = state.generated_messages.len(),
        cancelled = cancel.is_cancelled(),
    ),
)]
async fn run_step(...) { ... }
```

Per-phase fields:

| Span | Fields |
|------|--------|
| `agent_run` | `run_id` (UUID generated at run start), `prompt_count`, `model` (string), `provider` |
| `agent_repl_step` | `run_step` (index, monotonic), `intent` (e.g. `"ModelTurn"`), `context_messages`, `generated_messages` |
| `agent_read` | `intent` |
| `agent_eval` | `intent`, on close: `observation` (e.g. `"AssistantCollected"`, `"PreflightFailed"`), `terminal_error` (bool) |
| `agent_print` | `observation`, on close: `assistant_appended` (bool), `tool_results_appended` (count) |
| `agent_decide` | on close: `decision` (e.g. `"Continue(ExecuteTools)"`, `"Finish"`) |

### `Display` / `Debug` for intents and observations

`AgentIntent::kind()` returns a `&'static str` discriminant
(`"ModelTurn"`, `"ExecuteTools"`, etc.) so `tracing` doesn't
serialize entire enum payloads (which can include long message
arrays). Same for `AgentObservation::kind()`.

```rust
impl AgentIntent {
    fn kind(&self) -> &'static str {
        match self {
            Self::ModelTurn => "ModelTurn",
            Self::ExecuteTools { .. } => "ExecuteTools",
            Self::AppendFollowUps { .. } => "AppendFollowUps",
            Self::AppendSteering { .. } => "AppendSteering",
            Self::Finish => "Finish",
        }
    }
}
```

These are private helpers; they only exist to feed `tracing`.

### Run ID

Generate a `Uuid` once per `AgentLoop::run` call, stash it in
`AgentRunState`, and put it on the root span. This makes
filtering one run out of multi-run logs trivial:
`RUST_LOG=anie_agent=debug` plus `tracing-subscriber`'s
EnvFilter and field filtering picks it up.

The run ID is **not** persisted, **not** in `AgentRunResult`,
and **not** in any public event. It's a tracing-only field.

### What we don't add

- **No `AgentEvent::StepStart` / `StepEnd`.** The TUI does not
  need step boundaries to render; the protocol stays still.
- **No log records for every delta.** `MessageDelta` would spam
  logs at provider speed. Tracing happens at boundaries, not
  inside tight inner loops.
- **No metrics infrastructure.** Counters and histograms are
  separate work. If someone wants Prometheus later, they
  consume tracing events through a subscriber; the spans are
  the source of truth.

## Files to touch

- `crates/anie-agent/src/agent_loop.rs` — `#[instrument]`
  attributes on phase methods, `Span` creation in `run`,
  `kind()` helpers on `AgentIntent` / `AgentObservation`.
- `crates/anie-agent/Cargo.toml` — confirm `tracing` is already
  a workspace dep (it is — line 12 imports `tracing::warn`).

If `uuid` is not already a workspace dep, add it under a
`workspace = true` entry. Use `uuid = { version = "1", features
= ["v4"] }`. Check `cargo tree -p anie-agent` first.

Estimated diff: ~50 LOC (mostly attributes and field accessors).

## Test plan

Tracing is hard to test in unit tests without coupling to a
specific subscriber. Approach:

- **No new behavior tests.** PR 1's tests still pass; clippy
  still passes; that's the contract.
- **One observability test** in `agent_loop.rs::tests`: install
  a `tracing_test` (or in-memory `tracing-subscriber` layer),
  run a short scripted scenario, assert that the captured
  events include `agent_run` with `run_id`, at least one
  `agent_repl_step`, and matching span open/close pairs. Use
  `tracing-test` if it's a workspace-friendly choice — adding
  it just for this test is acceptable since it's a `dev-
  dependencies`-only crate.
- Manual: `RUST_LOG=anie_agent=debug cargo run --bin anie -- ...`
  and confirm spans appear.

If `tracing-test` adds friction, drop the observability test
and rely on the manual check; PR 4 is small enough that this
isn't a real risk.

## Risks

- **Span fields with large debug payloads slow tracing.**
  Mitigation: use `kind()` discriminants for intents and
  observations; never put full `Vec<Message>` into a span field.
- **`#[instrument]` interacts oddly with `async fn` and
  `&mut self`.** Mitigation: use `skip_all` and add fields
  explicitly. This is the standard pattern.
- **The tracing-subscriber test is brittle across `tracing`
  versions.** Mitigation: limit assertions to span names and
  presence of well-known fields; don't assert on exact event
  ordering across spans.

## Exit criteria

- [ ] `agent_run` span wraps every `AgentLoop::run` call.
- [ ] `agent_repl_step` span wraps every iteration of the
      driver loop.
- [ ] Each phase has its own span (`agent_read`, `agent_eval`,
      `agent_print`, `agent_decide`).
- [ ] Spans carry the documented fields.
- [ ] `RUST_LOG=anie_agent=debug` shows step progression in
      manual testing.
- [ ] PR 1's 14 characterization tests pass unchanged.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] No new public-API surface (private helpers only).

## Deferred

- Metrics layer (Prometheus, OpenTelemetry export).
- Per-tool spans (would require touching `execute_single_tool`).
- Per-provider spans (would require touching the provider
  trait — owned by `anie-provider`).
- TUI rendering of step boundaries. If the TUI ever needs this,
  it gets a separate `AgentEvent` variant in a future plan,
  not from PR 4's tracing.
