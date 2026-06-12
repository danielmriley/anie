# rlm2/PR2 — budget coupling + truncation alarm

## Rationale
README evidence: ceiling == num_ctx in the field today; Ollama
silently sheds the system prompt once the working set fills. The
content budget must derive from the allocation budget.

## Design
- `rlm_active_ceiling_tokens()` (controller.rs) becomes
  ceiling = explicit `ANIE_ACTIVE_CEILING_TOKENS` if set, else
  `effective_num_ctx − prompt_reserve − output_reserve`, floored at
  4_096. `prompt_reserve` = estimate of the composed system prompt +
  repo-map budget + 1k ledger slack (the controller has all three at
  build_rlm_extras time); `output_reserve` default 4_096
  (`ANIE_OUTPUT_RESERVE_TOKENS`).
- With the user's 16,384 num_ctx: ceiling ≈ 16,384 − ~3.5k − 4k ≈
  8.9k — the working set genuinely fits for the first time.
- Truncation alarm: when PR1's detector fires, emit a WARN log + a
  one-time SystemMessage ("Ollama evaluated fewer tokens than sent —
  context was silently truncated; lower the ceiling or raise
  /context-length") so the failure mode is visible instead of
  mysterious.

## Tests
- `default_ceiling_derives_from_effective_num_ctx_minus_reserves`
- `explicit_ceiling_env_still_wins`
- `ceiling_floor_holds_for_tiny_num_ctx`
- `truncation_alarm_fires_once_per_run`

## Risks
Reserve estimates are heuristic; floor + env overrides are the
escape hatches. Hosted (non-Ollama) models keep today's behavior.
