# rlm2/PR4 — summaries-first, sticky page-ins

## Rationale
Page-ins push full bodies (up to ceiling/4 per turn) at the END of
the working set (context_virt.rs:1199) — scrambled chronology, FIFO
displacement, re-eviction oscillation. The 66k navigation bill is
mostly this loop (verify against PR1 counters).

## Design
- Summaries-first: page in the Phase-F summary (or head-truncation)
  rather than the body; the ledger already tells the model `recurse`
  fetches the full body. Bodies page in only when no summary exists
  AND the body is < 512 tokens. (`ANIE_PAGE_IN_BODIES=1` restores
  old behavior for A/B.)
- Sticky set: a paged-in item stays until the latest user prompt
  changes (tracked by timestamp, like the prompt-embed cache), and
  is exempt from FIFO while sticky; the same item is never paged in
  twice for the same prompt.
- Per-run budget: total page-in spend capped (default 8k tokens,
  `ANIE_PAGE_IN_RUN_BUDGET`); PR1 counters expose it.
- Placement: paged-in summaries render inside ONE consolidated
  `<system-reminder source="archive-recall">` message adjacent to
  the ledger, not interleaved as fake user turns — chronology of
  the real transcript stays intact.

## Tests
- `page_in_prefers_summary_over_body`
- `sticky_page_in_survives_fifo_until_prompt_changes`
- `same_item_not_paged_twice_for_one_prompt`
- `per_run_page_in_budget_is_enforced`
- `paged_content_renders_in_one_archive_recall_message`

## Risks
Summaries may be too lossy for some answers — the recurse path and
the env escape hatch cover it; the corpus re-run arbitrates.
