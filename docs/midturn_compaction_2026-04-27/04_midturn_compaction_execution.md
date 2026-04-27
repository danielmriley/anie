# 04 — Mid-turn compaction execution

## Rationale

With plan 03's `CompactionGate` hook in place, the controller can
install an implementation that performs the codex-style mid-turn
compaction:

- After each sampling request inside an agent loop,
- if estimated context tokens exceed
  `context_window - effective_reserve` (plan 01), and
- the per-turn compaction budget (plan 02) is not exhausted,
- run `Session::auto_compact` against the supplied context,
- return the resulting messages to the agent loop, which uses
  them for the next sampling request.

Reference: `codex-rs/core/src/codex.rs:6452-6468`.

## Design

### Implementation: `ControllerCompactionGate`

A struct that holds the references the gate needs:

```rust
struct ControllerCompactionGate {
    session: Arc<Mutex<Session>>,
    summarizer: Arc<dyn MessageSummarizer>,
    config_provider: Arc<dyn Fn() -> CompactionConfig + Send + Sync>,
    budget: Arc<AtomicU32>, // shared with controller
    event_tx: mpsc::Sender<AgentEvent>,
}
```

- `session` is the same `Session` the controller manipulates. The
  agent loop has its own local `context` variable; the gate
  reconciles by passing the loop's context into a transient
  compaction call and returning the result. We do **not** mutate
  the persistent session here — that happens at turn end via the
  existing compaction-entry path. The gate is a "shrink this
  in-memory context" operation only for the in-flight loop.
- `config_provider` is a closure so the gate reads the latest
  values (e.g. user changed `/context-length` mid-session — the
  next mid-turn compaction sees the new effective reserve).
- `budget` is an `AtomicU32` shared with the controller so
  decrement/check is atomic. Reset by `run_prompt` per plan 02.

### Token estimate for the agent loop's context

The agent loop's local `context` is `Vec<Message>` (the
protocol-level type from `anie-protocol`). The existing session-
side helper `estimate_context_tokens`
(`crates/anie-session/src/lib.rs:1147`) takes a
`&[SessionContextMessage]` — the session-wrapped variant that
carries cached usage data. Different types; the existing helper
does not work on the gate's input directly.

The fix is a small primitive: a free function
`estimate_message_tokens(messages: &[Message]) -> u64` that
falls back to the content-based estimate already used inside
`estimate_context_tokens` when usage is unavailable. Add it
alongside the existing helper in `anie-session/src/lib.rs` in
PR A of this plan (so PR B's gate can call it directly), and
have `estimate_context_tokens` call into it for entries that
lack cached usage. Keeps the two callsites consistent.

### Gate logic

```rust
async fn maybe_compact(
    &self,
    context: &[Message],
) -> Result<CompactionGateOutcome, anyhow::Error> {
    let config = (self.config_provider)();
    let tokens = estimate_message_tokens(context);
    let threshold = config.context_window
        .saturating_sub(config.reserve_tokens);
    if tokens <= threshold {
        return Ok(CompactionGateOutcome::Continue);
    }
    if self.budget.load(Ordering::Acquire) == 0 {
        return Ok(CompactionGateOutcome::Skipped {
            reason: "compaction budget exhausted".into(),
        });
    }

    send_event(&self.event_tx, AgentEvent::CompactionStart).await;
    let result = compact_messages_inline(
        context,
        &config,
        self.summarizer.as_ref(),
    ).await?;
    self.budget.fetch_sub(1, Ordering::AcqRel);
    send_event(&self.event_tx, AgentEvent::CompactionEnd {
        summary: result.summary,
        tokens_before: tokens,
        tokens_after: estimate_message_tokens(&result.messages),
    }).await;

    Ok(CompactionGateOutcome::Compacted {
        messages: result.messages,
    })
}
```

### `compact_messages_inline`

A new helper alongside `Session::auto_compact` that operates on a
free-standing `&[Message]` rather than a session branch. The
existing `compact_internal`
(`crates/anie-session/src/lib.rs:1007-1014`) already does the
real work — extract the cut point, summarize older messages,
splice the summary in. Refactor to expose a pure function:

```rust
pub async fn compact_messages_inline(
    messages: &[Message],
    config: &CompactionConfig,
    summarizer: &dyn MessageSummarizer,
) -> Result<InlineCompactionResult>;

pub struct InlineCompactionResult {
    pub messages: Vec<Message>,
    pub summary: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
}
```

`Session::auto_compact` itself becomes a thin wrapper that calls
`compact_messages_inline` and then splices the result into the
session branch.

### Persisting the mid-turn compaction

Open question: does the mid-turn compaction need to persist into
the session log? Two options:

- **A. Don't persist mid-turn separately.** The compaction's effect
  is reflected in the next sampling request's context. The session
  log records the eventual final assistant message. The summary
  itself is regenerable from the original messages, but only as
  long as we can find them — and they're already in the session
  log up to the cut point. So skipping persistence is *recoverable*
  on replay.
- **B. Persist mid-turn compactions as branch entries**, same as
  pre-prompt ones. Future replay sees the exact context the agent
  saw on each iteration.

**Recommendation: A** for first landing. Mid-turn compactions are
ephemeral context shaping; the canonical session record is the
sequence of user prompts + assistant messages + tool calls/results,
all of which are already persisted independently. If we discover
replay fidelity issues, escalate to B in a follow-up.

### Cancellation

The gate must respect `CancellationToken`. The summarizer call
inside `compact_messages_inline` already accepts cancellation
through the existing strategy abstraction
(`CompactionStrategy::summarize`). We pass the same token from
the agent loop. If the user hits Ctrl+C mid-compaction, the
summarizer aborts; the gate returns
`Err(anyhow::Error::msg("cancelled"))`; the loop logs a warning
and continues with the original (unshrunk) context, which the
next sampling request will likely fail on, exiting the turn
cleanly.

## Files to touch

- `crates/anie-session/src/lib.rs`
  - Refactor `compact_internal` into a pure
    `compact_messages_inline` helper.
  - `Session::auto_compact` calls into the new helper, then does
    its own session-state splicing.
  - Add `pub fn estimate_message_tokens(messages: &[Message]) -> u64`
    that produces a content-based token estimate from
    protocol-level messages (no `SessionContextMessage` wrapping
    required). Have the existing
    `estimate_context_tokens(messages: &[SessionContextMessage])`
    delegate to it for content-only estimation when usage data
    is unavailable, so the two callsites stay consistent.
  - Public re-export of `compact_messages_inline`,
    `InlineCompactionResult`, and `estimate_message_tokens`.
- `crates/anie-cli/src/controller.rs`
  - Define `ControllerCompactionGate` struct.
  - In `build_agent` (line ~1179), apply
    `.with_compaction_gate(Arc::new(ControllerCompactionGate::new(...)))`
    on the `AgentLoopConfig` builder. Use the controller's session,
    summarizer, config closure, and shared atomic budget counter.
  - Wire the per-turn budget reset from plan 02 so the
    controller-side counter and the gate's atomic share storage
    (proposed: `Arc<AtomicU32>` lives on the controller; the gate
    holds an `Arc::clone` of it).

## Phased PRs

### PR A — Refactor `compact_internal` into a pure helper + token estimator

**Change:**

- New free function `compact_messages_inline`.
- New free function `estimate_message_tokens(&[Message]) -> u64`
  in `anie-session` for content-based token estimation on
  protocol-level messages.
- `Session::auto_compact` calls `compact_messages_inline`.
- `estimate_context_tokens` delegates to
  `estimate_message_tokens` for entries lacking cached usage.
- No behavioral change yet (no callers of the new helpers
  outside `Session::auto_compact` and the existing token-estimate
  callsite).

**Tests:**

- All existing compaction tests pass unchanged.
- New unit test for `compact_messages_inline` directly:
  `compact_messages_inline_summarizes_older_messages_and_keeps_recent`.
- New unit test for `estimate_message_tokens`:
  `estimate_message_tokens_falls_back_to_content_when_no_usage`.

**Exit criteria:**

- Test surface is the same; two new helpers exist for plan 04 PR B.

### PR B — Install `ControllerCompactionGate`

**Change:**

- Define the gate struct.
- Wire into `build_agent`.
- `AtomicU32` budget shared with the controller-side counter
  introduced in plan 02 PR A.

**Tests:**

- `midturn_compaction_fires_when_context_exceeds_threshold` —
  drive an agent through a fake provider that returns a huge tool
  call; verify the gate fired and the next request is built from
  the compacted context.
- `midturn_compaction_does_not_fire_under_threshold` — happy path
  without compaction.
- `midturn_compaction_skipped_when_budget_exhausted` — pre-set
  the budget to 0; verify the gate returns `Skipped` without
  running the summarizer.
- `midturn_compaction_cancellation` — fire Ctrl+C during
  compaction; verify clean turn exit.

**Exit criteria:**

- A small-context model with a tool-heavy prompt no longer
  surfaces `ContextOverflow` mid-turn — it compacts proactively.

### PR C — Manual smoke + documentation

**Change:**

- Add a section to `docs/notes/` (or wherever runtime tuning is
  documented) describing mid-turn compaction, the budget, and the
  effective-reserve formula.
- Manual smoke: run a small-context Ollama model on a coding task
  that reads several large files; verify the activity row shows
  "compacting Xs" mid-turn at least once and the turn completes
  successfully.

**Tests:**

- Documentation only.

**Exit criteria:**

- Documented behavior matches code; smoke run completes.

## Test plan

Beyond the per-PR lists, integration tests in
`anie-integration-tests`:

- `agent_session_midturn_compaction_drops_older_tool_results` —
  end-to-end with a fake provider.
- `agent_session_midturn_compaction_preserves_in_flight_tool_call_correlation`
  — if a tool call's results are about to be compacted away
  while another tool's results are still pending, the surviving
  correlation IDs must remain valid in the new context.

## Risks

- **`compact_internal` refactor risk.** Touching the existing
  compaction primitive risks subtle behavioral changes for the
  pre-prompt path. Mitigation: PR A keeps the existing tests
  unchanged and the refactor is split out from any new-caller
  changes.
- **In-flight tool-call correlation.** Compacting away an
  earlier tool-result message while a downstream message
  references that tool-call ID is a real failure mode. The
  existing `find_cut_point` heuristic already chooses cut points
  that don't sever assistant→tool-result pairs; we should verify
  that property holds for mid-turn input shapes too.
- **Cost of `TranscriptReplace`.** Re-rendering the full
  transcript every mid-turn compaction is expensive. Acceptable
  because compaction is rare and budget-bounded.

## Exit criteria

- [ ] Mid-turn compaction fires and prevents `ContextOverflow`
      on a small-context model with a tool-heavy prompt.
- [ ] Tool-call correlation survives mid-turn compaction.
- [ ] Cancellation during compaction exits cleanly.
- [ ] No regression on pre-prompt compaction tests.
- [ ] `cargo test --workspace`, clippy clean, manual smoke
      passing.

## Deferred

- **Persisting mid-turn compactions to the session log** (option
  B above). Revisit if replay fidelity issues surface.
- **Backoff after a no-op compaction.** If
  `tokens_after >= tokens_before`, the compaction didn't help —
  we should probably skip further compactions for the rest of
  the turn. Currently relies on the budget. A more nuanced
  "if compaction didn't free at least 25 % of context, stop
  trying for this turn" heuristic could land in a follow-up.
