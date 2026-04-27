# 09 — TUI agent-event drain bounds

## Rationale

The TUI drains queued agent events into a batch to avoid redrawing once
per token. That coalescing is good, but the review found the drain is
unbounded per frame. Under a very large burst of streaming deltas or
tool updates, the TUI can spend too long draining and processing a
single batch before returning to terminal input.

This relates to, but does not replace, the completed TUI responsiveness
work. That work improved render cost and urgent input behavior. This
plan bounds the amount of agent-event processing done before yielding
back to rendering/input.

## Design

Process agent events in bounded batches:

- maximum events per frame, and/or
- maximum drain time per frame

Prefer an event-count cap first because it is deterministic and easy to
test. Add a time budget only if profiling shows event size varies too
much for a count cap.

The existing coalescing behavior should remain inside each bounded
batch. If more events remain after the cap, schedule another prompt
render/poll cycle rather than spinning until the channel is empty.

Initial cap proposal:

- process the first blocking/ready event
- drain up to 256 additional queued events
- render/yield
- continue on the next tick if more events are queued

Tune the number with the existing TUI render benchmarks if needed.

## Files to touch

- `crates/anie-tui/src/app.rs`
  - Bound the agent-event drain loop in `run_tui`.
  - Preserve existing batch handling and coalescing semantics.
- `crates/anie-tui/src/tests.rs`
  - Add tests for bounded drain behavior if the event loop can be
    isolated.
- `docs/tui_responsiveness/` or execution notes
  - Reference this as a follow-up if it overlaps with prior TUI perf
    plans.

## Phased PRs

### PR A — Event-count bound

**Change:**

- Introduce a named constant for max drained agent events per frame.
- Stop draining once the cap is hit.
- Ensure the loop remains responsive and continues processing remaining
  events later.

**Tests:**

- A synthetic burst larger than the cap is split across multiple
  batches.
- Event order is preserved.
- Existing coalescing still occurs within a batch.

**Exit criteria:**

- No single TUI frame can drain an unbounded number of agent events.

### PR B — Benchmark/tune

**Change:**

- Run existing TUI render benchmarks or add a small benchmark case if
  one already fits the benchmark harness.
- Tune the cap if 256 is too high/low.

**Tests:**

- Existing TUI tests.
- Existing TUI benchmark smoke command if available.

**Exit criteria:**

- Cap is justified by either tests, benchmark output, or documented
  reasoning.

## Test plan

- `cargo test -p anie-tui`
- `cargo bench -p anie-tui --bench tui_render -- --warm-up-time 1 --measurement-time 3`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke: long streaming response while typing rapidly in the TUI.

## Risks

- Too-small batches can increase total redraws and reduce streaming
  smoothness.
- Too-large batches preserve the starvation risk.
- Do not reorder agent events while batching.

## Exit criteria

- TUI event processing has an explicit per-frame bound.
- Streaming remains smooth while terminal input stays responsive.

