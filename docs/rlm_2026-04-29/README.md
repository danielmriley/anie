# Recursive language models for anie

A multi-stage plan to bring Recursive Language Model (RLM)
capabilities into the anie agent harness. Motivated by the
paper [Zhang, Kraska, Khattab — *Recursive Language Models*,
arXiv 2512.24601](https://arxiv.org/abs/2512.24601). The
headline result that drives this work:

> RLM-Qwen3-8B (an 8B model post-trained for recursion)
> outperforms vanilla Qwen3-8B by 28.3% on average across
> long-context tasks, and approaches vanilla GPT-5 quality on
> three of them. RLM as a paradigm processes inputs up to two
> orders of magnitude beyond model context windows.

That's the story we want for anie's small-model users. The
REPL refactor (`docs/repl_agent_loop/`) was the foundation;
this folder is the capability work that builds on it.

## Why now

Three things lined up:

1. **The REPL machine is in place.** `AgentRunMachine` lets us
   spawn nested agent runs cleanly — exactly what RLM needs.
   `BeforeModelPolicy` lets us inject context-management
   tools without protocol churn.
2. **The compaction work proved the limits of the
   "fit-everything-in-context" paradigm.** We've spent real
   effort raising compaction budgets, soft-truncating web
   reads, and tuning context windows. RLM is the structural
   answer those tactical fixes were dancing around.
3. **The paper's RLM-Qwen3-8B result is the strongest signal
   we have** that the right harness can lift small local
   models into frontier-quality territory on long-context
   work. That's the entire reason anie exists.

## Unified vision: leftover-gated context paging

The plan series started as RLM `recurse` + context
virtualization (Plan 06). Virtualization — a lean ceiling
and an addressable overflow store — is the plumbing. The
product is the admit rule:

> **A chunk enters the active window only if it
> residual-reduces the current working sheet, or if it is
> an orphan failure.** Keyword / embedding similarity
> against the raw prompt is input-space catchment and is
> not the default. `recurse` is an escape hatch, not the
> page-in mechanism.

See [leftover_gated_paging.md](leftover_gated_paging.md).
Phase B reliability items (profiles, prompt presets,
parse-repair, evidence brief) are out of scope.

Plan 06 is the engineering roadmap that built the ceiling,
store, ledger, and recall renderer. Plan 07 is the
measurement infrastructure.

## Scope of the plan series

Eight plans, organized by purpose:

| # | Plan | Purpose | Branch |
|---|------|---------|--------|
| 01 | [Stagnation detection + aggressive compaction](01_stagnation_detection.md) | Endgame for the compaction-only paradigm; safety net while RLM ships | `main` (landed) |
| 02 | [RLM `recurse` tool (shape 1)](02_recurse_tool.md) | The model's interface to external context | `dev_rlm` (Phase A of 06) |
| 03 | [RLM recurse intent (shape 2)](03_recurse_intent.md) | Promotes recursion to a first-class loop step | deferred (revisit after 06 + 07 data) |
| 04 | [Native RLM compat (shape 3)](04_native_rlm_compat.md) | Profile for natively-recursive models | speculative |
| 05 | [Passive context management](05_passive_context_management.md) | Background summarization + JIT filtering — components Plan 06 composes | absorbed into 06 |
| **06** | [**Phased path to context virtualization**](06_phased_implementation.md) | Ceiling + store + ledger (plumbing). Admit rule: [leftover-gated paging](leftover_gated_paging.md) | `dev_rlm` |
| 07 | [Evaluation harness + mode flags](07_evaluation_harness.md) | `--harness-mode {baseline,current,rlm}`; scenario corpus; scoring | `dev_rlm` (parallel to 06A) |
| — | [Execution tracker](execution/README.md) | — | — |

## Reading order

Read in this order:

1. **This README** — the unified picture (leftover-gated paging).
2. **[Leftover-gated paging](leftover_gated_paging.md)** — the admit rule.
3. **Plan 06** — the phased roadmap that built the plumbing.
4. **Plan 07** — how we measure whether each phase worked.
5. **Plan 02** — Phase A's spec sheet (`recurse` as escape hatch).
6. **Plan 05** — components of Phases C–E (referenced from
   06; not standalone work anymore).
7. **Plan 01** — already shipped; the compaction work that
   serves as the fallback under the new paradigm.
8. **Plans 03, 04** — deferred / speculative; revisit after
   Plan 06 phases land + Plan 07 produces eval data.

## A note on context scope

Reading the paper, the RLM paradigm is framed around long
*input* prompts: the user hands the LLM a 5MB document and
the harness lets the LLM navigate it via tools. anie's daily
problem is different — it's not "user pasted a huge
document," it's "the run accumulated 30 turns of tool output
and prior assistant messages, and the active context is now
full of stale content."

The paradigm doesn't actually distinguish between the two.
"Long prompts as part of an external environment" applies
equally to:

- the user's prompt (paper's case study),
- accumulated session history,
- files in the working directory,
- prior sessions on disk.

Plan 02 (the `recurse` tool) targets **run-accumulated
context** as the primary external environment, with file/
session-history extension as a natural follow-up. That's
where anie's leverage is.

## Guiding principles

1. **Compaction is a band-aid; leftover-gated paging is the
   answer.** The stagnation detector + aggressive compaction
   (Plan 01) buys time within the old paradigm. We're not
   investing further in "fit more in context" beyond Plan 01.
2. **Harness-owned admit, model-owned recurse as hatch.**
   Keyword/embedding JIT is not the page-in predicate. A
   `recurse` tool remains so the model can fetch a body the
   harness did not admit.
3. **Eval-driven from Plan 02 onward.** The eval suite from
   `docs/small_model_capability_ideas_2026-04-29.md` (Tier 3
   #10) becomes load-bearing here. Without it we can't tell
   if shape 1 actually helps; with it, every later shape's
   priority is data-driven rather than theory-driven.
4. **Don't break the small wins.** PR 1 of the REPL refactor
   set the precedent: behavior characterization tests are
   the contract. Each plan in this series preserves the REPL
   tests from `crates/anie-agent/tests/agent_loop_*.rs` and
   the policy/machine surface they lock down.

## Out of scope

- **Post-training a recursive model.** The paper's
  RLM-Qwen3-8B is a fine-tuned variant. We're a harness, not
  a trainer; if the natively-recursive model becomes
  available on Ollama or HuggingFace we'll add a profile for
  it (Plan 04) but we won't train it ourselves.
- **Replacing compaction entirely.** Compaction stays in the
  loop as a fallback for runs where the model doesn't use
  `recurse` (or where it isn't installed). Plan 01 is the
  endpoint of compaction work, not the beginning of more.
- **Tree-of-thoughts-style branching.** Distinct paradigm.
  May appear later as a separate plan if we ever care about
  candidate-and-score search; not relevant to long-context
  navigation.

## Reference

- The paper: [arXiv 2512.24601](https://arxiv.org/abs/2512.24601),
  Zhang, Kraska, Khattab. Code:
  [github.com/alexzhang13/rlm](https://github.com/alexzhang13/rlm).
- Companion ideas doc:
  `docs/small_model_capability_ideas_2026-04-29.md`. The RLM
  work supersedes "Tier 3 #12 tiered context retrieval" in
  that doc — RLM is a stronger version of the same idea.
- The substrate: `docs/repl_agent_loop/` and the resulting
  `AgentRunMachine` + `BeforeModelPolicy` in
  `crates/anie-agent/src/agent_loop.rs`.
