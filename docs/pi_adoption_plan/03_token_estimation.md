# Plan 03 — provider-reported token usage for compaction triggering

**Tier 1 — tiny, drop-in accuracy improvement.**

## Rationale

anie uses a pure chars/4 heuristic for token estimation
(`crates/anie-session/src/lib.rs::estimate_tokens`). That
heuristic is what decides whether to trigger compaction via
`shouldCompact`-equivalent: `context_tokens > context_window -
reserve_tokens`.

pi does the same heuristic AND additionally seeds the running
count from provider-reported `usage.totalTokens` when it's
present on an assistant turn
(`packages/coding-agent/src/core/compaction/compaction.ts:135`).
Trailing messages (after the last reported usage) get the
heuristic, but anything before that point uses ground truth.

The upshot: pi's compaction triggers are more accurate on the
common case (every LLM response carries usage data). anie is
routinely off by 10-30% in either direction because chars/4
doesn't account for tokenizer specifics — JSON-heavy tool-call
traffic over-estimates, CJK text under-estimates.

## Design

Replace `estimate_context_tokens` (or its caller) with a
two-phase walk:

1. Walk messages newest → oldest.
2. For each assistant message, check `message.usage.total_tokens`.
   If populated, use it as the running total for everything
   preceding that message (no need to walk further back).
3. Everything from that message forward (toward newest) gets the
   existing chars/4 heuristic.

Net cost: the walk terminates earlier on the common case. Output
value is a better estimate. No changes to compaction triggering
logic; only the estimate function changes.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | `estimate_context_tokens` uses usage-seeded walk. |
| `crates/anie-session/src/lib.rs` tests | New tests for the hybrid estimate. |

## PR

Single commit:

1. Refactor `estimate_context_tokens` to walk newest → oldest.
2. For each `Message::Assistant`, check `usage.total_tokens`.
3. When found: return `total_tokens + trailing_heuristic_sum`.
4. When not found: fall back to full heuristic sum (current
   behavior).
5. Ensure `shouldCompact`-equivalent uses the new function.

## Test plan

| # | Test |
|---|------|
| 1 | `estimate_context_tokens_uses_heuristic_when_no_usage_available` (regression) |
| 2 | `estimate_context_tokens_seeds_from_latest_assistant_usage` |
| 3 | `estimate_context_tokens_adds_trailing_messages_via_heuristic` — assert a user turn after a usage-reporting assistant sums correctly |
| 4 | `estimate_context_tokens_prefers_newer_usage_over_older` — two assistant turns with usage; the newer one is used |
| 5 | `estimate_context_tokens_ignores_stale_usage_on_compacted_branch` — after a Compaction entry, usage from before it shouldn't matter because those messages are discarded |

## Risks

- **Usage field reliability.** Some providers under-report
  (prompt caching can confuse totals). Mitigation: if
  `total_tokens` > 2× our heuristic, fall back to the heuristic
  — cap the trust.
- **Model-switch mid-session.** Different models tokenize
  differently; a usage number from model A doesn't directly
  translate to model B. We don't care at the compaction-triggering
  granularity (both are "about this many tokens") but if it
  becomes an issue, reset the seed on a `ModelChange` entry.

## Exit criteria

- [ ] Hybrid estimator lands.
- [ ] Tests 1-5 pass.
- [ ] No regressions in existing compaction tests.

## Deferred

- **Real tokenization** via `tiktoken-rs` or similar. Drop-in
  possible but adds a dependency. Revisit if the hybrid estimate
  still produces noticeable errors.
