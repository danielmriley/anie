# 03 — Agent-loop compaction signal

## Rationale

The agent loop in `crates/anie-agent/src/agent_loop.rs:355+` is
self-contained: it owns the `context: Vec<Message>` for the turn,
calls the provider, processes tool calls, appends responses, and
loops. The controller has no hook to interject between iterations.

Mid-turn compaction (plan 04) needs exactly that hook. Following
codex's pattern (`codex-rs/core/src/codex.rs:6420-6468`), the
natural insertion point is *after a sampling request completes*,
before the next iteration's request is built. At that boundary
the agent has just finished applying any tool results to its
context; the controller can read the current token estimate and
decide whether compaction is warranted before the agent burns
another sampling request on a too-large prompt.

This plan introduces the wiring **without** introducing any
compaction behavior. After this PR lands, the controller has a
hook it can use; using it is plan 04's job.

## Design

### Shape of the hook

A new optional callback on `AgentLoopConfig`:

```rust
pub trait CompactionGate: Send + Sync {
    /// Called by the agent loop after each sampling request that
    /// produced an assistant message and before deciding whether
    /// to issue another. Implementations may inspect / mutate the
    /// supplied context and return whatever it should be after
    /// any compaction. Returning `None` means "context unchanged;
    /// continue."
    async fn maybe_compact(
        &self,
        context: &[Message],
    ) -> Result<CompactionGateOutcome, anyhow::Error>;
}

pub enum CompactionGateOutcome {
    /// No compaction needed; continue with the same context.
    Continue,
    /// Compaction ran; replace the loop's context with `messages`.
    Compacted { messages: Vec<Message> },
    /// Budget exhausted (or another reason); skip compaction this
    /// time, continue. The agent loop should NOT treat this as
    /// an error — it's a deliberate "don't compact now."
    Skipped { reason: String },
}
```

- `Result<…, anyhow::Error>` rather than a typed error keeps the
  trait simple. Real failures (summarizer returned garbage, etc.)
  bubble as terminal turn errors via the existing
  `terminal_error` field on `AgentRunResult`.
- `Compacted { messages }` is the load-bearing variant. It hands
  the loop a fresh context to use for the next iteration.
- `Skipped { reason }` keeps a clean audit trail when budget is
  exhausted (plan 02). The reason string flows into telemetry
  (plan 06).

### Where the hook fires

In `AgentLoop::run`'s loop (`agent_loop.rs:355+`), after the
provider call returns and the assistant response has been
appended to `context`, but *before* tool execution. Tool calls
themselves don't change the context size yet (results are added
after execution); the size grows when tool results come back.
So the hook should fire **after tool results have been merged
into `context`** but **before the next sampling iteration starts
to build its request**.

Concretely the call site is the top of each `loop` iteration after
the first, where the agent decides whether to issue another
sampling request. Existing local variable: `context: Vec<Message>`.

```rust
if let Some(gate) = &self.config.compaction_gate {
    match gate.maybe_compact(&context).await {
        Ok(CompactionGateOutcome::Continue) => {}
        Ok(CompactionGateOutcome::Compacted { messages }) => {
            context = messages;
            send_event(event_tx, AgentEvent::TranscriptReplace {
                messages: context.clone(),
            }).await;
        }
        Ok(CompactionGateOutcome::Skipped { reason }) => {
            send_event(event_tx, AgentEvent::SystemMessage {
                text: format!("Skipped mid-turn compaction: {reason}"),
            }).await;
        }
        Err(error) => {
            // Don't kill the turn on a hook failure — log and
            // continue. The next sampling request may still
            // overflow, and the reactive path will handle it.
            tracing::warn!(?error, "compaction gate failed");
        }
    }
}
```

### `TranscriptReplace` semantics

Anie already has `AgentEvent::TranscriptReplace { messages }`
(handled at `crates/anie-tui/src/app.rs:847`). Reusing it for
mid-turn keeps the UI side identical: the output pane clears
and reloads from the supplied messages, just like after a
pre-prompt compaction.

### Default behavior

`AgentLoopConfig::compaction_gate` defaults to `None`. The check at
the top of the loop is a single `Option::is_some` branch — zero
cost for any callers that don't install one. Provider tests,
`anie-integration-tests`, and the CLI's `print` mode all pass
`None`.

## Files to touch

- `crates/anie-agent/src/agent_loop.rs`
  - Define `CompactionGate` trait and `CompactionGateOutcome`
    enum at module level.
  - Add `compaction_gate: Option<Arc<dyn CompactionGate>>` to
    `AgentLoopConfig`. Match the existing builder pattern by
    adding `with_compaction_gate(...)` (mirrors
    `with_ollama_num_ctx_override`).
  - Insert the hook call at the top of the loop body after the
    first iteration (i.e., when we're about to issue another
    sampling request).
- `crates/anie-cli/src/controller.rs`
  - `build_agent` (line ~1179) constructs `AgentLoopConfig::new(...)`
    and applies builder methods. No behavioral change in this PR —
    do not call `with_compaction_gate`. Plan 04 will install a real
    implementation here.

## Phased PRs

This plan is one PR. The change is small and structural; phasing
would make the diff harder to review without buying anything.

### PR — Add `CompactionGate` trait + agent-loop hook (default off)

**Change:**

- `CompactionGate` trait, `CompactionGateOutcome` enum.
- `AgentLoopConfig::compaction_gate` field.
- Hook call site in `AgentLoop::run`.
- Existing `build_agent` in `controller.rs` passes `None`.

**Tests:**

- `agent_run_calls_compaction_gate_between_iterations` — install
  a stub gate that returns `Continue` once, `Compacted { ... }`
  the next time. Drive the agent through a multi-iteration tool
  call. Assert the gate was called the expected number of times
  and that `Compacted` replaced the context.
- `agent_run_with_no_gate_behaves_like_today` — pass `None`,
  verify the run is byte-identical to a baseline.
- `agent_run_continues_when_gate_errors` — gate returns `Err`,
  the agent still finishes the turn (with the next sampling
  request potentially overflowing, but that's handled
  elsewhere).

## Risks

- **Async trait object behind an `Arc`.** `async-trait` already
  in the workspace handles this. No new dep.
- **Replacing `context` mid-loop is observable.** Tool calls
  reference message IDs / call IDs from prior turns. Compaction
  must preserve any in-flight tool-call correlation. The
  existing pre-prompt compaction already handles this via
  `find_cut_point`; we reuse the same primitive in plan 04, so
  the correctness rests on the same foundation.
- **`TranscriptReplace` cost.** Re-rendering the entire
  transcript is expensive (full block-cache rebuild). Acceptable
  because compaction is rare; budget-bounded by plan 02.

## Exit criteria

- [ ] `CompactionGate` trait exists and is exercised by tests.
- [ ] Default behavior (`compaction_gate: None`) is byte-identical
      to today.
- [ ] `cargo test --workspace` and clippy clean.
- [ ] `anie-integration-tests` agent-session tests still green.

## Deferred

- **Pre-tool-call hook.** A more aggressive version would also
  check before dispatching a tool that's known to produce large
  output. Speculative; revisit if mid-turn compaction proves
  insufficient on its own.
- **Streaming-aware hook.** Hook does not fire mid-stream — same
  reason as the broader plan: the model's context is locked
  while a sampling request is in flight.
