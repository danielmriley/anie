# rlm2/PR3 — batch eviction + append-only turns

## Rationale
Per-turn ReplaceMessages (strip ledger → evict → page-in → new
ledger) changes the prompt prefix every turn → full re-prefill on
Ollama → 247s scenario wall clocks (README). Turns that don't NEED
eviction must be byte-stable appends.

## Design
- Low-water mark: when `running_total > ceiling`, evict down to
  `ceiling × 0.6` (env `ANIE_EVICT_LOW_WATER_PCT`) in one batch, so
  subsequent turns append without evicting.
- No-op fast path: when nothing was archived, evicted, or paged in
  AND the ledger content is byte-identical to the previous fire,
  return `Continue` instead of `ReplaceMessages` (the existing
  ledger message stays in place — do not strip/re-append identical
  bytes).
- Ledger stability: entries render in stable insertion order,
  append-only between evictions; the ledger remains strictly the
  last message.

## Tests
- `under_ceiling_turn_with_no_new_archive_returns_continue`
- `eviction_batches_down_to_low_water_mark`
- `ledger_bytes_stable_across_appending_turns`
- `ledger_remains_last_message_after_page_in`

## Risks
The no-op path must still archive new messages (store-side) even
when it returns Continue — archiving is store-only and does not
require mutating the working context.
