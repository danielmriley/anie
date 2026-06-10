# PR 4 — Relevance-based eviction of failed tool results

## Rationale

Cursor's harness post-mortem
([cursor.com/blog/continually-improving-agent-harness](https://cursor.com/blog/continually-improving-agent-harness))
calls out a failure mode they've seen across deployments:

> "Tool call errors remain in context, wasting tokens
> and causing 'context rot,' where accumulated
> mistakes degrade the quality of the model's
> subsequent decisions."

PR 1 (failed-tool-result wrap) prevents the model
from hallucinating PAST a failure — but the failure
still sits in active context indefinitely, taking
tokens and degrading decisions on later turns. PR 4
addresses the second half of the problem:
once a failure is stale, evict it.

The first instinct is "evict after N turns" — but
this is brittle. A failure that's still relevant N
turns later (the model is still fixing the bug it
caused) shouldn't be evicted; a failure that's no
longer relevant after 1 turn (the model moved
entirely to a different task) shouldn't wait.

Relevance, not turn count, is the right signal.

## Design

A failed `ToolResult` becomes an eviction candidate
when **either** of the following holds:

### Signal A: supersession

A subsequent successful tool call has the same
`(tool_name, args_hash)` as the failed one. This
means the model retried with identical args and
succeeded — the failure record adds no information.

The args_hash machinery is already in place from PR 2
(`crates/anie-agent/src/failure_loop.rs`). PR 4
extends it: each successful tool result also gets
hashed; the controller checks whether any earlier
failed result has matching `(tool_name, args_hash)`
and marks it superseded.

### Signal B: low relevance to current turn

For each failed `ToolResult` that's been in active
context for ≥2 turns, compute its embedding
similarity to the most recent user message. If the
similarity is in the bottom quartile of all active-
context messages, mark it as eviction-eligible.

The embedding infrastructure is already in place from
Plan 08 (`crates/anie-cli/src/embedder.rs` +
`bg_embedder.rs`). The reranker uses these embeddings
to decide what to page IN; PR 4 uses them to decide
what to page OUT.

When `ContextVirtualizationPolicy` next runs an
eviction pass, eviction-eligible failed results go
first — before the standard FIFO order kicks in.
They're not deleted: they go to the external store
like any other evicted content, accessible via
recurse if the model needs them later.

### Why both signals, not just one

Signal A is precise but narrow — it only catches the
"retried with identical args and succeeded" pattern.
Signal B catches "moved on to a different task"
which is the more common case (most failed tool
calls aren't retried-and-succeeded; they're just
left behind as the model adapts).

Together they cover both patterns. Either one alone
leaves real cases on the table.

## Files to touch

- `crates/anie-cli/src/external_context.rs` — track
  `(tool_name, args_hash)` for both successful and
  failed tool results. Currently only failures are
  hashed (PR 2). Extend to all tool results.
- `crates/anie-cli/src/context_virt.rs` — extend
  `ContextVirtualizationPolicy` with a
  `mark_supersedable_failures` pass that runs
  before the standard FIFO eviction. Add the
  embedding-based relevance check.
- `crates/anie-cli/src/embedder.rs` — expose a
  helper for "rank these messages by similarity to
  this prompt" (today's reranker computes this for
  paging-in; we need the same shape for paging-out).
- Tests in `anie-cli`.

Estimated diff: ~250 LOC of code, ~150 LOC of tests.

## Phased PRs

Single PR, but the two signals could be split:
- 4.1 — Supersession-based marking only.
- 4.2 — Embedding-relevance marking on top.

If the implementation gets unwieldy, split. The
default plan is to ship them together.

## Test plan

- `failed_tool_result_evicted_when_same_args_succeed_later`
  — supersession signal fires; failed result evicted
  ahead of FIFO order.
- `failed_tool_result_kept_when_subsequent_success_has_different_args`
  — supersession only triggers on exact arg match.
- `failed_tool_result_evicted_when_embedding_similarity_low`
  — relevance signal fires when failure's
  similarity ranks bottom quartile.
- `failed_tool_result_kept_when_referenced_in_recent_turn`
  — relevance signal does NOT fire when the
  failure's content is still being discussed.
- `failed_tool_result_eviction_respects_pin_for_user_messages`
  — pinned messages (PR rlm/17 user-message pin)
  are never evicted, even if their similarity is
  low.
- `eviction_policy_falls_back_to_fifo_when_no_supersedable_or_low_relevance_failures`
  — backwards compat: behavior unchanged when no
  failures are eviction-eligible.

## Risks

- **Mistaken eviction.** A failed result that the
  model would have referenced later, evicted because
  similarity scoring missed the connection. The
  failure is still in the archive (recurse can fetch
  it), so this is recoverable but adds latency.
  Mitigation: signal B requires similarity in the
  bottom quartile (not just below median); only
  consistently-irrelevant failures evict.
- **Embedding cost.** Computing similarity on every
  turn for every active-context message. Mitigation:
  the embedder runs in the background already (PR
  rlm/20); cache hits will be common since the same
  messages tend to stay active across consecutive
  turns.
- **Not enough effect.** If the smoke shows context
  rot still degrades performance even with PR 4
  evicting failures, the signal is too weak. Followup
  could expand to evict ANY low-relevance content,
  not just failures.

## Exit criteria

- [ ] Both signals (supersession + relevance)
      implemented.
- [ ] All six tests above pass.
- [ ] `cargo test --workspace` + clippy clean.
- [ ] Smoke run shows failed results getting paged
      out faster on tasks with topic switches (T10
      pivot in the smoke protocol).
- [ ] Eviction count visible in the rlm ledger line
      (`evicted N (M failed)` segment).
- [ ] `ANIE_DISABLE_FAILURE_EVICTION=1` turns the new
      behavior off entirely (FIFO only).

## Deferred

- Generalizing to non-failure low-relevance content.
  Today the active-context FIFO keeps the most-recent
  N turns regardless of relevance; this PR only
  addresses failures. If smoke shows wins from
  relevance-based eviction, extending to all message
  types is a natural follow-up.
- A user-facing "show me what got evicted" command
  for transparency.
- Tuning the bottom-quartile threshold per-model.
  Cursor's "per-tool, per-model baselines" pattern
  applies here too — defer until we have real
  multi-model usage.
