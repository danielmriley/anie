# Local fusion — best-of-N with deterministic selection

Source idea: OpenRouter's **fusion router**
(https://openrouter.ai/docs/guides/routing/routers/fusion-router) —
the model decides if a task warrants deliberation → a panel of up to
8 models answers in parallel → a **judge** emits structured analysis
(consensus / disagreement / coverage gaps / blind spots) as JSON, not
a merge or a pick → the original model synthesizes the final answer.
Cost ~4-5× per 3-model panel, linear in panel size.

This doc adapts that for **local models in anie**. The adaptation is
not a faithful port: three things invert going from cloud to local,
and one inversion makes the local version *better* for coding.

## The three cloud→local inversions

1. **Cost → throughput, NOT serial latency.** OpenRouter's 4-5× is
   dollars per completion. Locally completions are ~free, and — the
   key fact the design rests on — **Ollama batches concurrent
   requests against one loaded model** (continuous batching via
   `OLLAMA_NUM_PARALLEL`). Verified on the dev machine 2026-06-14:
   the live `llama-server` runs `-np 5 -c 81920` = 5 parallel slots ×
   16,384 ctx each. So a best-of-N panel runs *concurrently* on the
   single GPU, sharing weights — wall-clock is far below N× (the
   requests compete for compute, so each is somewhat slower, but it
   is throughput cost, not additive latency). anie already issues
   concurrent model requests in `parallel_decompose` (tokio::spawn +
   join), so the plumbing exists.

2. **Multi-model panel → sampling/approach diversity on ONE model.**
   OpenRouter mixes gpt/claude/gemini. Locally, running different
   models means model-swap thrash (the exact problem the
   `docs/rlm_context_v2` + summarizer/embedder campaigns fixed).
   The local-native panel is **one loaded model, N candidates** via
   temperature/seed spread (self-consistency / best-of-N) — and the
   temperature-perturbation plumbing already exists (plan 03 PR10
   loop-escalation). Optional richer diversity: seed each candidate
   with a different *approach* via the existing decompose pass.

3. **Model-judge → tests-as-judge.** OpenRouter's judge is a strong
   model writing prose analysis. A *local* judge is a small model,
   and we measured (docs/edit_completion_guard, classifier accuracy)
   how unreliable small models are at judgment. For **coding**, anie
   doesn't need a model judge: it has the verify policy
   (`crates/anie-cli/src/verify_runner.rs`). Generate N candidate
   patches, run each through the project's tests / typecheck, pick
   the one that passes. The judge is the compiler — strictly more
   reliable than cloud fusion's model-judge, and it's the
   deterministic-over-generative principle the whole harness follows.

## The Ollama parallelism budget (the binding constraint)

Continuous batching is real but **memory-bound on consumer VRAM**:
`-c` (total KV cache) = `num_ctx` × `num_parallel`. On the dev box
(6 GB VRAM) 5 slots × 16k ctx already forces heavy CPU offload for
the 12b model. So panel size is bounded by:

```
panel_size ≤ floor(available_kv_budget / per_candidate_num_ctx)
```

This couples directly to the rlm/num_ctx work:
- A best-of-N panel where each candidate needs the full agent
  context (repo state + task) pays N × that context in KV. Memory,
  not compute, caps the panel.
- The PR19 default-num_ctx clamp and the rlm-derived ceiling
  (`docs/rlm_context_v2`) set per-candidate context; the fusion layer
  must read the *effective* num_ctx and size the panel to fit the KV
  budget, degrading to fewer candidates (or panel_size=1 = today's
  behavior) rather than OOM-ing.
- `parallel_decompose::safe_max_concurrency` currently CLAMPS Ollama
  concurrency to 1 by default (a conservative freeze-era guard). The
  fusion feature must coordinate with / revisit that clamp, gated on
  the measured KV budget rather than a blanket "1".

## Design (anie-native): best-of-N with verify selection

A high-stakes-step feature, gated — NOT a per-turn default.

1. **Gate** (reuse the edit-guard's model-judged gating pattern):
   engage only when (a) explicitly requested (`--fusion` /
   `[fusion].enabled`, set by offline/benchmark runs), or (b) a
   high-stakes step the model flags (a final patch on a hard task).
   Default off; rlm-only.
2. **Panel**: N candidates from the loaded model, issued
   concurrently to fill Ollama's parallel slots. Diversity source:
   temperature spread (cheap, reuse perturbation), optionally
   approach-seeded via decompose. N sized to the KV budget (above).
3. **Select — deterministic first**: run each candidate's patch
   through the verify command (tests/typecheck). Keep the candidates
   that pass. Among passers, prefer the smallest diff (SWE-bench
   convention: smallest change that resolves). If zero candidates
   verify (or no verify command configured), fall back to a
   model-judge tiebreak, then to the single best-scoring candidate.
4. **Synthesize**: apply the winner. If nothing verified, the run
   degrades to the normal single-shot outcome — fusion never makes
   the result worse than today.
5. **Instrument**: RunMetrics `fusion { engaged, panel_size,
   verified_candidates, selected_by (tests|judge|fallback),
   wall_clock_overhead_ms }` so the benchmark measures the lift AND
   the real batched wall-clock cost.

## Honest caveats (load-bearing — these set the sequencing)

- **Multiplier, not fixer.** Best-of-N amplifies an *existing
  non-zero* per-attempt success probability by harvesting variance.
  It does ~nothing for a task the model is at 0 on (N correlated
  tries fail alike). Today a single anie attempt produces a *correct*
  patch ~1/25 and often no patch at all. Fusion is worth its
  throughput cost only AFTER the floor rises — which is what the
  edit guard, tool reliability, and context work are doing. Build
  fusion to harvest the "model can sometimes do this" tail, last.
- **Memory-bounded panel.** On 6 GB VRAM, the realistic panel for a
  full-context coding task is small (2-3), not 8. Measure before
  assuming.
- **Contention.** Firing a panel while the main agent loop also
  needs the model competes for the same slots; the gate must account
  for this (don't fuse mid-turn on top of other load).
- **We already run this pattern at the orchestration layer.** Every
  adversarial-review workflow this project runs IS fusion (N
  independent attempts + a verification step). The eval
  mode-comparison proves multi-attempt value. The open question is
  whether internalizing it into the agent loop beats leaving it as
  orchestration — answerable only with the benchmark numbers.

## Sequencing & exit criteria

- **Prerequisite**: a per-attempt base rate high enough to multiply.
  Gate this plan on the edit-guard benchmark result (does the
  non-empty/resolved rate move?). If a single attempt still resolves
  ~1/25, defer fusion; the floor isn't ready.
- [ ] best-of-N (panel sized to KV budget, tests-as-judge) beats
      single-shot resolved rate on the n=25 subset by a margin that
      justifies the measured batched wall-clock overhead.
- [ ] fusion never lowers the resolved rate vs single-shot (the
      degrade-to-single-shot fallback holds).
- [ ] panel sizing respects the KV budget (no OOM / no crash) across
      e4b and 12b on the dev box.

## PR sketch (when sequenced)

- **PR1**: KV-budget panel sizing + concurrent candidate issue
  (reuse parallel_decompose plumbing; revisit safe_max_concurrency).
- **PR2**: diversity (temperature spread; optional decompose seeds).
- **PR3**: verify-as-judge selection + smallest-diff tiebreak +
  degrade-to-single-shot.
- **PR4**: gating (config/flag, high-stakes detection) + RunMetrics
  fusion block + benchmark `--fusion` measurement pass.

## Deferred / not this

- Faithful "structured analysis" judge (consensus/blind-spots JSON)
  for non-verifiable tasks — needs a strong judge anie doesn't have
  locally; revisit only with a hosted-judge escape hatch (out of the
  pure-local scope).
- Cross-model panels (swap thrash) — sampling diversity first.
