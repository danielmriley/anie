# Plan 04 — Native RLM compat (shape 3)

**Branch:** TBD.
**Status:** speculative; tracked here so it isn't lost.
Ship only if a natively-recursive small model becomes
available on a backend anie supports.

## Rationale

The paper post-trains an 8B model (RLM-Qwen3-8B) to be
*natively* recursive — its training included examples of
calling itself with sub-queries, examining external prompts,
aggregating sub-results. Code is at
[github.com/alexzhang13/rlm](https://github.com/alexzhang13/rlm).
The published numbers:

> RLM-Qwen3-8B beats vanilla Qwen3-8B by 28.3% on average
> across long-context benchmarks; approaches vanilla GPT-5
> on three of four.

If that model (or a successor) becomes available on Ollama
or another local backend anie supports, we want anie to
recognize it and switch into RLM mode automatically.

## Design

### A new model compat flag

`ModelCompat` (in `anie-provider`) gains a variant:

```rust
pub enum ModelCompat {
    None,
    Minimal,
    // ... existing variants ...
    Rlm,
}
```

Models tagged `Rlm` get a different harness configuration:

- **Different system prompt.** The natively-recursive model
  expects a recursion-aware framing, not the generic coder
  prompt anie ships today.
- **Different tool surface.** `recurse` is *primary* —
  promoted in the tool list, not just present.
- **Different default `BeforeModelPolicy`.** Native RLM
  models prefer to navigate context themselves rather than
  receive a pre-baked repo map. The default policy
  becomes a noop; the model uses `recurse(file=...)` when
  it wants the file.
- **Larger recursion budget by default.** Native models
  use recursion liberally — bump the per-run budget from
  16 (shape 1 default) to e.g. 64.
- **Possibly: different system-prompt for sub-agents.** A
  natively-recursive sub-agent likely shouldn't have its
  recursion path locked off; remove the `max_depth`
  exclusion or raise the cap.

### Model catalog entries

In `anie-config`'s default model catalog, add entries for
known natively-recursive models when they become available:

```toml
[[providers.ollama.models]]
id = "rlm-qwen3:8b"
name = "RLM Qwen3 8B (natively recursive)"
context_window = 32768
compat = "Rlm"
# ... etc ...
```

### Profile loading

Add a `ModelProfile` struct keyed off `ModelCompat`. The
profile holds:

```rust
pub struct ModelProfile {
    pub system_prompt_template: SystemPromptTemplate,
    pub default_recursion_budget: u32,
    pub default_max_depth: u8,
    pub default_before_model_policy: BeforeModelPolicyKind,
}
```

When the controller builds an `AgentLoop` for a run, it
consults the profile keyed off the selected model's compat
flag and applies the profile's defaults — overridable by
explicit user config.

This is the same shape as the per-model prompt templates
idea from `docs/small_model_capability_ideas_2026-04-29.md`
(Tier 1 #2). Plan 04 is the natural endpoint of that work.

## When this becomes worth doing

When all three are true:

- A natively-recursive small model is accessible from one of
  anie's backends (Ollama, OpenRouter, etc.). As of
  2026-04-29 this is the open-source RLM-Qwen3-8B from the
  paper; we'd need it published to a model registry anie
  uses.
- Plan 02 (recurse tool) has shipped and the recurse path
  is exercised in the eval suite.
- Plan 03 (recurse intent) has either shipped or been
  explicitly skipped — Plan 04 builds on the same surface.

## Risks

- **Model availability.** As of writing, RLM-Qwen3-8B is in
  a research repo, not on Ollama. Plan 04 is contingent on
  that landing.
- **Backend feature support.** Ollama's tool-calling
  support is reliable, but a natively-recursive model might
  expect specific structured-output formats anie doesn't
  yet emit. The constrained-decoding follow-up from the
  small-model ideas doc (Tier 3 #11) may need to land
  first.

## Exit criteria

To be filled in if/when unblocked. At minimum:

- `ModelCompat::Rlm` exists in `anie-provider`.
- The default model catalog includes at least one
  natively-recursive model.
- A `ModelProfile` for `Rlm` ships and the controller
  consults it at agent-loop build time.
- The eval suite includes `Rlm` model entries and shows
  the expected delta vs. base model + recurse-tool harness.

## Deferred / open questions

- Whether `Rlm` should imply *both* "use the RLM model" and
  "harness behaves differently," or whether they should be
  separate flags. Today's `ModelCompat` is shape-of-API,
  not capability-of-model — but RLM blurs that line.
- Whether to support an "RLM-mode" override that turns on
  RLM-style harness behavior for non-natively-trained
  models. Probably yes for advanced users who want the
  harness to recurse aggressively even on a non-RLM model.
