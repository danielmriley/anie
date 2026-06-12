# rlm context v2 — pay-as-you-go context management

Make the rlm context-virtualization pipeline a near-zero-cost no-op
below the ceiling, escalating only under real pressure — and fix the
live budget-overflow bug.

## Evidence (2026-06-12)

- Corpus matrix (`docs/local_model_augmentation/execution/lift_e4b.json`,
  gemma4:e4b, 11 scenarios × {current, rlm}): rlm wins exactly its
  target scenarios (repo-map cold start 19k→4k tokens; verify loop)
  but **taxes multi-turn navigation**: `find_compaction_stats` burned
  66k tokens / 12 calls / 247s under rlm vs 11k / 1 call under
  current. Total: 276k tokens (rlm) vs 197k (current).
- **Live bug**: ceiling (16,384) == user num_ctx override (16,384),
  but requests also carry the system prompt (~1.4k), repo map (~1k),
  ledger, and need output room. Ollama context-shifts silently when
  full — discarding the OLDEST tokens, i.e. the system prompt. No
  num_ctx↔ceiling check exists anywhere; `prompt_eval_count` arrives
  every turn and is never compared against what we sent.
- Mechanics verified in code: page-ins are `working.push()`
  (`context_virt.rs:1199,1215`) — archived OLD messages appended
  after the newest turns (scrambled chronology), young in FIFO so
  they displace other content, then age out and become candidates
  again (oscillation). Eviction + ledger strip/rebuild mutate the
  prompt prefix every turn, forcing full re-prefill on Ollama.

## Principles

1. **Instrument before optimizing.** PR1 lands counters; every later
   PR must move them on the corpus, or it reverts.
2. **Pay-as-you-go.** A turn under the ceiling is byte-stable
   (append-only) — zero eviction, zero page-in churn, identical
   ledger bytes when nothing changed. Prefix-cache discipline is a
   latency feature on local hardware, not an aesthetic.
3. **Pull over push.** The archive's full bodies are one `recurse`
   away; the harness pushes summaries, not bodies.
4. **The allocation contract and the content contract are one
   contract.** The ceiling derives from what Ollama will actually
   serve, minus what the prompt and output need.

## PRs (sequential — they share context_virt.rs)

| PR | Plan | Summary |
|----|------|---------|
| rlm2/PR1 | [01_instrumentation.md](01_instrumentation.md) | RunMetrics `context` block: evictions, page-in tokens, ledger tokens, prefill telemetry, truncation detector input |
| rlm2/PR2 | [02_budget_coupling.md](02_budget_coupling.md) | Ceiling derived from effective num_ctx; silent-truncation alarm |
| rlm2/PR3 | [03_hysteresis.md](03_hysteresis.md) | Batch eviction to a low-water mark; append-only turns; stable ledger |
| rlm2/PR4 | [04_page_in_v2.md](04_page_in_v2.md) | Summaries-first page-in, sticky set, per-run budget |
| rlm2/PR5 | [05_diet_and_knobs.md](05_diet_and_knobs.md) | Ledger caps, token-budgeted tail, size-aware eviction, cached token sets |

## Exit criteria (series)

- [ ] Corpus matrix re-run (gemma4:e4b, both modes): rlm total tokens
      ≤ 1.2× current on the navigation family; `find_compaction_stats`
      rlm wall clock < 120s.
- [ ] `context.truncation_suspected == 0` across the corpus with
      default config (the P1 bug class is structurally gone).
- [ ] rlm retains its wins (repo_map_cold_start, verify_broken_fixture).
- [ ] No regression on the qwen3.5:0.8b time-scenario field result.
