# PR 7 — First policy boundary: before-model

**Goal:** Add one extension point to `AgentRunMachine`: a
`BeforeModelRequest` policy hook that fires in the `Read` phase
of every `ModelTurn` step. Default behavior is a noop — no
real consumer is wired up in this PR. The point is to *land*
the extension shape and prove it doesn't break anything.

This is the first PR in the series that introduces a new
capability. It's intentionally small and gated: only the hook
shape lands, with one tested noop default and one tested
"append messages" implementation.

## Rationale

After PR 6, the machine is the production agent loop. The
architecture doc identifies before-model as the most valuable
first hook because it unlocks (in future PRs):

- **Context augmentation.** Retrieve file/symbol summaries
  before the model sees the prompt.
- **Proactive compaction.** Compact when context approaches a
  threshold, before the provider returns an overflow error.
- **Repo-map staging.** Inject a tiered project map between the
  user prompt and the model.
- **Queued user steering.** Fold a queued correction into the
  next `Read` phase at a safe boundary.

PR 7 does *not* implement any of those. It lands the seam and a
trivial `Append` implementation as proof of concept. The first
real consumer (likely proactive compaction or queued-prompt
folding) gets its own plan.

The architecture doc warns:

> For the first policy extension, prefer a very small default-
> noop hook or an append-messages-only hook. Actual session
> compaction should remain controller/session-owned unless a
> later plan deliberately moves that boundary.

PR 7 follows that guidance precisely.

## Design

### Public types

```rust
// crates/anie-agent/src/agent_loop.rs (or new policy.rs submodule)

pub struct BeforeModelRequest<'a> {
    pub context: &'a [Message],
    pub generated_messages: &'a [Message],
    pub model: &'a Model,
    pub step_index: usize,
}

pub enum BeforeModelResponse {
    /// Proceed with the model request unchanged.
    Continue,
    /// Append these messages to the run context before the
    /// model request goes out. Useful for context augmentation
    /// and proactive compaction summaries.
    AppendMessages(Vec<Message>),
}

#[async_trait]
pub trait BeforeModelPolicy: Send + Sync {
    async fn before_model(
        &self,
        request: BeforeModelRequest<'_>,
    ) -> BeforeModelResponse;
}

/// Default noop policy — current behavior unchanged.
pub struct NoopBeforeModelPolicy;

#[async_trait]
impl BeforeModelPolicy for NoopBeforeModelPolicy {
    async fn before_model(
        &self,
        _request: BeforeModelRequest<'_>,
    ) -> BeforeModelResponse {
        BeforeModelResponse::Continue
    }
}
```

> If `async_trait` isn't already a workspace dep, prefer a
> non-async trait (`fn before_model(&self, ...)
> -> BeforeModelResponse`) for PR 7 to avoid pulling in a new
> dep for a noop hook. Async can come back when a real consumer
> needs it.

### Wiring into `AgentLoop`

`AgentLoopConfig` (currently constructed in
`crates/anie-agent/src/agent_loop.rs:280-..`) gets one new
field:

```rust
pub struct AgentLoopConfig {
    // ... existing fields ...
    pub before_model_policy: Arc<dyn BeforeModelPolicy>,
}
```

With a `Default::default()` (or a field-skip plus a constructor
default) of `Arc::new(NoopBeforeModelPolicy)` so existing
callers don't have to set the field. Match how
`follow_up_provider` is currently wired — same `Arc<dyn _>`
pattern, same default-noop pattern.

### Wiring into the machine

In the `Read` phase for `AgentIntent::ModelTurn`:

```rust
async fn read_model_request(
    &self,
    state: &mut AgentRunState,
) -> ReadResult {
    // 1. Run before-model policy.
    let request = BeforeModelRequest {
        context: state.context(),
        generated_messages: state.generated_messages(),
        model: &self.config.model,
        step_index: state.step_index(),
    };
    match self.config.before_model_policy.before_model(request).await {
        BeforeModelResponse::Continue => {}
        BeforeModelResponse::AppendMessages(messages) => {
            state.append_policy_context(messages);
        }
    }

    // 2. Existing read logic: resolve options, build context, etc.
    // (unchanged from PR 3)
}
```

Note: `Read` becomes `&mut state` here because it now mutates
on `AppendMessages`. That's a meaningful change from PR 3's
"Read takes `&state`" rule, and is acceptable because the
mutation is bounded and goes through a state helper.

`AgentRunState::append_policy_context(messages)` is a new helper
that appends to `context` *only*, not to `generated_messages`.
Policy-injected messages aren't user-generated — they're
runtime-augmented context, and the controller doesn't persist
them as session output.

> If we decide later that policy messages *should* be
> persisted, we change `append_policy_context` then. The point
> of the helper is that the rule is in one place.

### Tests for the noop and an example consumer

Two test policies in
`crates/anie-agent/tests/agent_loop_policy.rs`:

1. **`noop_policy_preserves_existing_behavior`**: drive a
   scenario with `NoopBeforeModelPolicy` and the same scenario
   with no policy field; assert event sequences and
   `AgentRunResult` are equal. (This proves the hook
   integration didn't change behavior.)
2. **`append_policy_injects_messages_into_context`**: a test-
   only `AppendOncePolicy { msgs: Vec<Message> }` that returns
   `AppendMessages(msgs.clone())` on the first call and
   `Continue` thereafter. Drive a scenario; assert the appended
   messages appear in `final_context` *before* the assistant
   response, and do *not* appear in `generated_messages`.

PR 1's 14 characterization tests must still pass with the
default noop policy.

## Files to touch

- `crates/anie-agent/src/agent_loop.rs` — add types (or
  `policy.rs` submodule + re-export), wire field into
  `AgentLoopConfig`, call hook in Read.
- `crates/anie-agent/src/agent_loop.rs` (state) — add
  `append_policy_context` helper.
- `crates/anie-agent/src/lib.rs` — re-export
  `BeforeModelPolicy`, `BeforeModelRequest`,
  `BeforeModelResponse`, `NoopBeforeModelPolicy`.
- `crates/anie-cli` — set
  `before_model_policy: Arc::new(NoopBeforeModelPolicy)`
  explicitly in the agent-loop config, or rely on `Default`. If
  the controller currently builds the config struct field-by-
  field, prefer explicit so the seam is visible.
- `crates/anie-agent/tests/agent_loop_policy.rs` — new tests.

Estimated diff: ~150 LOC (types + wiring + tests).

## Test plan

- PR 1's 14 characterization tests pass with default noop.
- The 2 new policy tests pass.
- `cargo test --workspace`.
- `cargo clippy --workspace --all-targets -- -D warnings`.

Manual smoke is *not* required for PR 7 — the noop default is
strictly equivalent to PR 6's behavior. The smoke from PR 6
covers the live-LLM path.

## Risks

- **The trait shape locks in too early.** Mitigation: PR 7 is
  the *first* policy hook. We pick a shape that fits one
  use case (append-context). When the second hook arrives
  (after-model? on-tool-error? mid-stream?), we may want
  to refactor the trait into a unified `AgentPolicy` with
  multiple methods. Accept that — the cost of refactoring one
  trait with two implementations is low; the cost of
  speculatively designing for hooks we haven't needed yet is
  high.
- **`AppendMessages` injects messages that confuse the
  provider.** Mitigation: in PR 7 there's no real consumer, so
  this is theoretical. Tests use messages with valid roles and
  shapes. The plan for the first real consumer must verify
  provider compatibility per-provider.
- **`step_index` semantics.** Mitigation: define it as "0 for
  the first ModelTurn intent in the run, incrementing per
  REPL iteration" — matches PR 4's tracing field of the same
  name. Tests assert it's monotonic.
- **`Send + Sync` on the trait constrains future use.**
  Mitigation: `BeforeModelPolicy` is held in `Arc<dyn _>`
  inside `AgentLoopConfig`, which is already shared across
  threads. The bounds match `follow_up_provider`'s — this is
  consistent.

## Exit criteria

- [ ] `BeforeModelPolicy` trait, `BeforeModelRequest`,
      `BeforeModelResponse`, `NoopBeforeModelPolicy` are public.
- [ ] `AgentLoopConfig::before_model_policy` exists with a
      noop default.
- [ ] The Read phase calls the policy hook before resolving
      provider options for `ModelTurn` intents.
- [ ] `AgentRunState::append_policy_context` appends to
      context only, not to generated messages.
- [ ] PR 1's 14 characterization tests pass with default noop.
- [ ] PR 7's 2 policy tests pass.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] No `AgentEvent` variants added.
- [ ] `docs/arch/anie-rs_architecture.md` mentions the policy
      hook (one or two sentences).

## Deferred

- After-model, on-tool-error, before-tool, after-stream-error
  hooks. Each gets its own plan when a consumer needs it.
- A unified `AgentPolicy` trait. Refactor when the second hook
  lands.
- Wiring `before_model_policy` into the controller for
  proactive compaction or queued-prompt folding. Each gets its
  own plan.
- Persistence of policy-injected messages into session
  history. Out of scope; today they're context-only.
- Async trait support (if PR 7 ships sync). Add when a
  consumer needs an `.await`.
