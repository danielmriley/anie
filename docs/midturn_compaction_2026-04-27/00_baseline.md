# 00 — Baseline analysis: anie vs pi vs codex compaction

This document is **reference material**, not a PR. It captures the
state of the three implementations at the moment this plan set was
written so future contributors can verify the synthesis without
re-walking the cross-codebase grep. Cite specific lines when
updating.

## Anie today

### Pre-turn auto-compaction

`InteractiveController::run_prompt` calls `maybe_auto_compact` after
appending the user message and before the agent runs:

- `crates/anie-cli/src/controller.rs:625-632` — appends prompt entry,
  invokes `maybe_auto_compact` if `compaction.enabled`.
- `crates/anie-cli/src/controller.rs:925-954` — `maybe_auto_compact`
  body. Estimates `tokens_before`, compares against
  `context_window - reserve_tokens`. If under threshold, returns
  silently. Otherwise emits `CompactionStart`, calls
  `Session::auto_compact`, emits `CompactionEnd`.
- `crates/anie-session/src/lib.rs:875-890` — `Session::auto_compact`
  itself: re-checks the threshold (defensive double-gate), then
  delegates to `compact_internal`.
- `crates/anie-session/src/lib.rs:1007-1014` — `compact_internal`
  picks a cut point via `find_cut_point(messages,
  keep_recent_tokens)`, summarizes the older portion, splices a
  summary message into the branch.

Defaults (`crates/anie-config/src/lib.rs:295-300`):

- `reserve_tokens: 16_384`
- `keep_recent_tokens: 20_000`
- threshold: `context_window - reserve_tokens`

### Reactive overflow compaction (already exists)

`ProviderError::ContextOverflow` exists and the retry policy
already routes it through compaction:

- `crates/anie-provider/src/error.rs:31-34` — variant.
- `crates/anie-providers-builtin/src/util.rs:26` — classification
  shim that builds `ContextOverflow` from a 4xx body.
- `crates/anie-cli/src/retry_policy.rs:85-100` — `decide` returns
  `RetryDecision::Compact` for `ContextOverflow` unless we have
  already compacted on this attempt, in which case
  `GiveUp::AlreadyCompacted`.

So pi-style "model returned overflow → compact + retry once" is
already in place, with a guard against infinite retry loops.

### What anie does **not** have

- **Mid-turn proactive compaction.** Once `agent.run` is in its
  inner sampling loop (`crates/anie-agent/src/agent_loop.rs:355`+),
  there is no callback that lets the controller compact between
  iterations. The loop just keeps appending to its local `context`
  variable and dispatching the next provider call.
- **A compaction-budget per user turn.** `maybe_auto_compact` and
  the reactive path each may compact once, but nothing enforces a
  combined cap. In a degenerate case, a turn could compact via
  `maybe_auto_compact`, then hit overflow on the next sampling
  request, then compact again, repeatedly, with no anti-thrash
  bound.
- **Adaptive reserve.** `reserve_tokens` is a fixed default 16,384
  regardless of the configured `context_window`. For an 8K-context
  model this means the trigger is `8192 - 16384 = 0` (saturating
  to zero), which would compact every turn unconditionally.
- **Adaptive tool-output sizing.** Tool output caps in
  `crates/anie-tools` are configured globally, not against the
  effective context window. A 64K-byte bash output is the same
  whether anie is talking to a 4K-context tinyllama or a
  200K-context Sonnet.

## Pi today

### Compaction trigger sites

`packages/coding-agent/src/core/agent-session.ts:1755-1833` —
`_checkCompaction(assistantMessage, skipAbortedCheck)` is the
single entry point. Called from two locations:

- Line 579: at agent end (after the agent loop completes).
  Catches both threshold and overflow cases retroactively.
- Line 1024: before sending the next user prompt. Catches an
  overflow that happened on a turn whose final assistant message
  was the error itself.

### Branches inside `_checkCompaction`

1. **Overflow** (line 1782-1804): the LLM returned a context-
   overflow error. Drop the failed assistant message from agent
   state, set `_overflowRecoveryAttempted`, run
   `_runAutoCompaction("overflow", true)`, retry.
2. **Threshold** (line 1830-1832): context tokens exceed the
   configured threshold. Run
   `_runAutoCompaction("threshold", false)`.

### What pi does **not** have

- **Mid-turn proactive compaction.** Like anie, pi's agent loop
  runs to completion (or to error) before any compaction check
  fires.

So the pi/anie shape is essentially the same on this question.

## Codex today

### Mid-turn compaction site

`codex-rs/core/src/codex.rs:6420-6468` — after each sampling
request inside the agent loop:

```rust
let total_usage_tokens = sess.get_total_token_usage().await;
let token_limit_reached = total_usage_tokens >= auto_compact_limit;
// ...
if token_limit_reached && needs_follow_up {
    if run_auto_compact(
        &sess,
        &turn_context,
        InitialContextInjection::BeforeLastUserMessage,
        CompactionReason::ContextLimit,
        CompactionPhase::MidTurn,
    )
    .await
    .is_err()
    {
        return None;
    }
    client_session.reset_websocket_session();
    can_drain_pending_input = !model_needs_follow_up;
    continue;
}
```

Key observations:

- Trigger is `total_usage_tokens >= auto_compact_limit` *and*
  `needs_follow_up` (more sampling required for tool follow-ups
  or pending input). If the agent is about to terminate the turn
  anyway, no compaction is needed.
- Compaction runs synchronously on the agent task. The websocket
  is reset (relevant to codex's transport, not anie). The loop
  then `continue`s, re-reading the now-shorter context.
- Errors from `run_auto_compact` terminate the turn — there is no
  retry-the-compaction path. The user sees the failure, not a
  hang.

### Phase enum

`codex-rs/core/src/compact.rs:17` imports
`codex_analytics::CompactionPhase`. The variants observed in
codex's source include `MidTurn` and `StandaloneTurn`. The
`compact.rs` module also references pre-turn variants in
comments. Codex uses this enum for analytics; anie can adopt a
similar enum (or a free-form `CompactionReason` parameter) for
events and logging without committing to codex's analytics
pipeline.

### Anti-thrash

Codex relies on the assumption stated in the comment at line 6451:
*"as long as compaction works well in getting us way below the
token limit, we shouldn't worry about being in an infinite loop."*
Anie's small-context targets violate that assumption — at 8K
context, a single tool result can push back into the red on the
very next iteration. We need an explicit per-turn budget where
codex relies on dimensional headroom.

## Synthesis

| Capability | Anie today | Pi | Codex |
|---|---|---|---|
| Compact before next user prompt | yes | yes | yes |
| Compact at end of agent run | no (folded into pre-prompt of next turn) | yes | n/a |
| Reactive on `ContextOverflow` | yes (single retry) | yes (single retry) | n/a (mid-turn prevents it) |
| Mid-turn proactive compaction | **no** | no | **yes** |
| Per-turn compaction budget | implicit (one reactive retry) | one reactive retry | none (relies on headroom) |
| Adaptive reserve to context window | no | partial (uses model's context window directly) | n/a (large-window assumption) |
| Adaptive tool output caps | no | partial | partial |

This plan set targets the bolded gaps and the small-context
sizing that neither reference implementation handles, since
neither targets local hardware as primarily as anie does on the
ground.
