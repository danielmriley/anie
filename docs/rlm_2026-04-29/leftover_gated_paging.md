# Leftover-gated context paging

**This is the main harness bet on `dev_rlm`.** A lean active
window plus an addressable overflow store is plumbing.
The product is the admit rule.

## Admit rule

A chunk pages into the active window only if:

1. **Residual-reduce** — its tokens hit leftover on the
   current working sheet (latest user goal ∪ in-window
   validation failures, minus tokens claimed by successful
   tool results), or
2. **Orphan** — it is a `ToolResult { is_error: true }` that
   nothing on the sheet yet explains.

Keyword overlap and embedding cosine against the raw prompt
are input-space catchment. They are **not** the default
admit predicate. `ANIE_PAGE_IN_ADMIT=keyword` restores that
hatch for A/B.

`recurse` stays available as an escape hatch when the model
needs a body the harness did not page in. It is not the
headline.

## What this is not

- Virtualization ≠ leftover-gated context. Ceiling + store
  without this admit rule still pages by similarity.
- Compaction that averages explained + unexplained into one
  summary is tactical only.
- Phase B reliability work (profiles, prompt presets,
  parse-repair, evidence brief) is out of scope for this
  change.
- No TaskState framework. The working sheet is goal +
  leftover tokens + claimed tokens.

## Where it lives

- Admit types + tests: `crates/anie-cli/src/leftover.rs`
- Policy (evict, pin, recall render): `crates/anie-cli/src/context_virt.rs`
- Mode: `--harness-mode=rlm` with a finite
  `ANIE_ACTIVE_CEILING_TOKENS`

## Tests that lock the rule

- `sheet_subtracts_claimed_evidence_from_goal`
- `residual_reduce_rejects_keyword_match_already_claimed`
- `residual_reduce_admits_unclaimed_leftover_tokens`
- `orphan_failure_admits_when_nothing_explains_it`
- `leftover_gated_page_in_admits_residual_not_claimed_keyword_match`
- `keyword_hatch_pages_claimed_prompt_overlap`
- `leftover_gated_page_in_admits_orphan_failure`
- `leftover_gated_rejects_embedding_only_match`
- `leftover_paging_preserves_latest_user_pin`
- `leftover_paging_still_evicts_over_ceiling`
- `page_in_admit_default_is_leftover_not_keyword`
