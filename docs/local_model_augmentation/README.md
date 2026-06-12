# Local-model augmentation — plan series

Make anie world-class as a harness for **small local models
(7–14B, served via Ollama)** by systematically compensating for
the specific ways small models fail. This series formalizes and
extends the Tier-1 sketches in
`docs/small_model_capability_ideas_2026-04-29.md` into three
phased, evidence-first plans.

## Scope decisions (agreed 2026-06-11)

- **Target tier**: small models first (Qwen3 8B/14B, Llama-8B
  class). Mitigations that work here help every tier above.
- **Pure local**: no paid-API escalation. Cascading between
  local models is deferred (see each plan's Deferred section).
- **Backend**: Ollama native `/api/chat` only
  (`crates/anie-providers-builtin/src/ollama_chat/`). llama.cpp
  GBNF grammar enforcement is out of scope for this series.
- **Breadth**: top three leverage areas only. Context-layout
  tuning, best-of-N sampling, and per-model prompt profiles are
  deferred to a follow-up series.

## Principles

1. **Deterministic before generative.** If the harness can fix,
   check, or compute something in code (argument coercion,
   running the build, attaching file context), it must not
   spend model turns on it. Small models burn turns; code
   doesn't.
2. **Spend local tokens, protect the context.** Local retries
   are nearly free; context pollution is not. Recovery
   mechanisms must be token-bounded and must not leave debris
   in the active context (follow the eviction precedents in
   `context_virt.rs`).
3. **Prefix-cache discipline.** Ollama reprocesses the prompt
   when the prefix changes. Anything injected must be either
   byte-stable across turns (repo map, few-shot examples) or
   strictly appended (verification results, ledgers). The
   `SystemPromptCache` mtime-stamp pattern
   (`crates/anie-cli/src/runtime/prompt_cache.rs`) is the
   template.
4. **Eval-driven.** Every feature lands with scenarios in
   `anie-evals` and counters in `RunMetrics`. A mitigation that
   doesn't move pass-rate, token, or turn metrics on the
   corpus gets reverted. The harness already supports mode
   comparison (`--modes baseline,current,rlm`); use it.
5. **Reuse existing seams.** New behavior composes through
   `BeforeModelPolicy` / `ChainedBeforeModelPolicy`
   (`crates/anie-agent/src/agent_loop.rs:383,427`), the typed
   `ProviderError` taxonomy, and existing deps (`ignore`,
   `grep-searcher`, `jsonschema`, the Ollama embedder). No new
   heavyweight dependencies (tree-sitter is explicitly
   deferred).

## The plans

| # | Plan | One-line summary |
|---|------|------------------|
| 01 | [Tool-call reliability](01_tool_call_reliability.md) | Schema-guided argument coercion, a bounded repair round for invalid calls, per-tool example calls. **PRs 1-4 shipped**; amended 2026-06-12 with unknown-tool rescue + unknown-prop stripping (PRs 16-17) |
| 02 | [Repo map + retrieval](02_repo_map_and_retrieval.md) | Pre-computed, token-capped repo skeleton injected at turn 0; on-demand symbol lookup tool |
| 03 | [Harness verification + failure recovery](03_harness_verification_and_recovery.md) | Harness-run verify command after edits; failure-loop escalation (temperature perturb); grounded edit-failure recovery. Amended 2026-06-12 with near-duplicate call detection (PR 18) |
| 04 | [Context-budget discipline](04_context_budget_discipline.md) | Prompt-weight tiers for small windows: compact catalogs, skills/context-file budgets, ledger v2 (PRs 13-15) |

Field evidence driving the amendments and plan 04:
[field_notes/2026-06-12_qwen3.5-0.8b_session.md](field_notes/2026-06-12_qwen3.5-0.8b_session.md)
— the first real small-model session (qwen3.5:0.8b): 36% tool-error
rate, a hallucinated tool name as the dominant failure, ledger syntax
leaking into bash commands, and an 11.3k-token turn-0 prompt against a
16.4k ceiling.

## PR ordering and dependencies

Recommended landing order (revised 2026-06-12, evidence-ranked by the
field session): **01 amendment (PRs 16-17) → 04 (PRs 13-15) → 03
(PRs 9-12, 18) → 02 (PRs 5-8)**.

- PRs 16-17 first: they deterministically rescue the failure class
  that dominated the field session (6 of 8 errors), are small, and
  build directly on shipped PR1-PR4 machinery.
- Plan 04 second: prompt weight is the systemic ceiling on every other
  improvement — nothing else matters if 69% of the window is spent at
  turn 0.
- Plan 03 then adds the write-side recovery + Signal C (PR 18 depends
  on PR 10's perturbation slot).
- Plan 02 (repo map) is unchanged but moves last: it *adds* prompt
  weight, so it should land after plan 04's tiering can budget for it.

Original ordering rationale below still applies within plans.

- Plan 01 is the highest measured-lift-per-line change (a
  malformed call costs a full turn on a 9B model today) and
  touches only the agent loop + one new module.
- Plan 03 PR 1 (verify command) is independent and can land in
  parallel with plan 01. Plan 03 PR 2 depends on plumbing
  `temperature` through the Ollama request body — currently
  **not forwarded at all**
  (`crates/anie-providers-builtin/src/ollama_chat/convert.rs:12-27`
  sends only `num_ctx`).
- Plan 02 is the largest scope (new module + policy + tool) and
  benefits from the eval corpus extensions landed by 01/03.
- `RunMetrics` schema bumps: plans 01 and 03 both add counters.
  Land each bump separately and sequentially (the schema has a
  single `schema_version: u32`,
  `crates/anie-evals/src/lib.rs:74-110`); coordinate in the
  execution tracker.

## Status

See [execution/README.md](execution/README.md). Update it as
PRs land, following the convention from
`docs/max_tokens_handling/` and `docs/skills_2026-05-02/`.

## Relationship to prior work

- `docs/small_model_capability_ideas_2026-04-29.md` — the idea
  inventory this series draws from (§1 repo map, §3 tool-call
  repair). Where this series deviates from a sketch there, the
  plan says so inline.
- `docs/harness_mitigations_2026-05-01/` — shipped read-side
  mitigations (failed-result wrapping, loop detector
  observability, failure eviction). This series adds the
  write-side: the harness *acts* instead of only advising.
- `docs/rlm_2026-04-29/` — context virtualization. Plans here
  assume rlm mode is the local-model profile and use its
  policies/env-gates as precedent.
