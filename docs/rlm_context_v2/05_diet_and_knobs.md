# rlm2/PR5 — ledger diet, token-budget tail, size-aware eviction

## Rationale
Remaining linear growth + blunt knobs (README P4/P5), plus the two
deferred perf-review findings that live in the same code.

## Design
- Ledger caps: most recent K=8 entries per tool + one "N earlier
  calls — recurse message_grep to search them" line; skip entries
  whose bodies are currently in the working set or sticky set.
- Token-budgeted pinned tail: replace positional keep_last_n with
  `pin_tail_tokens` (default 3_072), keeping at minimum the last
  user message + last assistant message regardless of size.
- Size-aware FIFO: among evictable messages, evict the oldest LARGE
  tool results first (>1k tokens), then standard FIFO — small
  assistant texts carry narrative continuity at negligible cost.
- Perf (deferred 2026-06-11 findings): cache per-message token sets
  at archive time keyed by MessageId; score page-in candidates by
  reference and clone only the selected items.

## Tests
- `ledger_caps_per_tool_with_overflow_line`
- `ledger_skips_entries_present_in_working_set`
- `pinned_tail_is_token_budgeted_not_positional`
- `large_old_tool_results_evict_before_small_text`
- `candidate_scoring_does_not_clone_unselected_bodies`

## Risks
keep_last_n is referenced in docs/tests broadly — keep it as a
deprecated alias mapping onto the token budget during transition.
