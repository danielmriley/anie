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

Two phases (pi's algorithm at
`packages/coding-agent/src/core/compaction/compaction.ts:~177`):

1. **Find the latest usage index.** Walk messages newest →
   oldest. Stop at the first `Message::Assistant` whose
   `usage.total_tokens > 0`. Record `(usage_total, index)`.
   If no such message is found, fall back to the pure heuristic
   (existing behavior).
2. **Add trailing heuristic.** Sum the chars/4 estimate for every
   message at `index + 1 ..= last`.

Total = `usage_total + trailing_sum`. The walk terminates early
on the common case (usage is almost always present on recent
assistant turns). The key design constraint: we don't accumulate
usage across multiple assistant messages — each `total_tokens`
reading represents the *whole prior context at that turn*, so
using the newest one IS the whole-history estimate.

Net cost: O(n) in the worst case (no usage anywhere, full
heuristic), O(k) in the common case where k is the number of
messages after the latest usage-reporting turn. Output value is
a better estimate. No changes to compaction triggering logic;
only the estimate function changes.

**Additions vs. pi.** Pi trusts `totalTokens` unconditionally. We
consider two guardrails, both deferred to a follow-up unless
needed:

- A 2× cap: if the reported usage exceeds 2 × the heuristic
  for the same messages, fall back. Protects against provider
  bugs.
- Model-switch reset: a `ModelChange` entry between two
  assistant turns implies different tokenizers and the seed
  from the older model may not reflect the new model's view.

Neither is in pi. Ship the hybrid estimator without them; add
guardrails if we observe wrong-side-of-threshold compaction
misfires in practice.

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
