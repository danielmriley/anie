# rlm2/PR1 — context instrumentation

## Rationale
Every later PR claims a token/latency win; without counters we are
guessing which mechanism (re-prefill vs page-in churn) dominates the
66k-token navigation tax (README evidence).

## Design
- `ContextVirtualizationPolicy` already emits
  `AgentEvent::RlmStatsUpdate`; extend it (additive, serde-defaulted)
  with per-fire deltas: `evicted_count`, `evicted_tokens`,
  `paged_in_count`, `paged_in_tokens`, `ledger_tokens`.
- RunMetrics v4→v5: `context { evictions, evicted_tokens, page_ins,
  page_in_tokens, ledger_tokens_total, prefill_tokens_total,
  truncation_suspected }`. `prefill_tokens_total` sums Ollama's
  per-turn `prompt_eval_count` (already mapped to
  `Usage::input_tokens`); `truncation_suspected` increments when
  `prompt_eval_count` < 0.9 × our `estimate_tokens` of the sent
  context (consumed by PR2's alarm; counted here).
- Evals mirror + runner_mock fixture bump + forward-compat test, as
  in every prior schema bump.

## Tests
- `rlm_stats_deltas_accumulate_into_context_metrics`
- `truncation_suspected_increments_when_prefill_undershoots_estimate`
- `v4_metrics_artifact_loads_with_context_block_defaulted`

## Risks
estimate_tokens is a heuristic (len/4); the 0.9 factor must tolerate
its error band — tune the factor, never alarm on hosted providers
(no prompt_eval_count semantics there).
