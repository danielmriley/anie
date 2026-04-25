# max_tokens handling: notes from pi, plan for anie

## The bug we hit

OpenRouter returned a 400:

```
maximum context length is 262144 tokens. However, you requested
about 263100 tokens (344 of text input, 612 of tool input,
262144 in the output).
```

The upstream reserves *one* budget for input + output against the
context window. Setting `max_tokens = context_window` guarantees
a rejection as soon as any input is sent.

We already landed a clamp (`max_tokens = min(advertised,
context_window / 2)`, commit `e85a117`) as a stopgap. It covers
the reported case but doesn't match how a principled agent should
manage the budget.

## How pi handles it

### 1. The main agent loop does not send `max_tokens`

`pi/packages/ai/src/providers/openai-completions.ts:394-400`:

```ts
if (options?.maxTokens) {
    if (compat.maxTokensField === "max_tokens") {
        (params as any).max_tokens = options.maxTokens;
    } else {
        params.max_completion_tokens = options.maxTokens;
    }
}
```

The `max_tokens` / `max_completion_tokens` field is **only
emitted when the caller explicitly sets `options.maxTokens`**.
Callers that set it, from a grep of
`pi/packages/coding-agent/src/`:

- `compaction/compaction.ts::generateSummary`
  — `maxTokens = Math.floor(0.8 * reserveTokens)` for bounded
  summarization output.
- `compaction/compaction.ts::generateTurnPrefixSummary`
  — `maxTokens = Math.floor(0.5 * reserveTokens)`.
- `compaction/branch-summarization.ts`
  — `maxTokens: 2048` for a bounded branch summary.

**The main agent stream never sets `maxTokens`.** The upstream
uses its own default — which is always compatible with the
context window because the upstream owns the invariant
`input + output <= context`.

### 2. `Model.maxTokens` in pi's catalog is a hint, not a wire value

`pi/packages/coding-agent/src/core/model-registry.ts:557`:

```ts
maxTokens: modelDef.maxTokens ?? 16384,
```

The catalog entry has a `maxTokens` field, but nothing in the
main agent loop reads it and forwards it onto the wire. It
exists for operators who want to reference or override it, and
for the compaction summarization path (which doesn't use the
catalog value anyway — it derives from `reserveTokens`).

### 3. Budget management lives in compaction, not in request building

`pi/packages/coding-agent/src/core/compaction/compaction.ts:115-125`:

```ts
export interface CompactionSettings {
    enabled: boolean;
    reserveTokens: number;
    keepRecentTokens: number;
}

export const DEFAULT_COMPACTION_SETTINGS: CompactionSettings = {
    enabled: true,
    reserveTokens: 16384,
    keepRecentTokens: 20000,
};
```

And `shouldCompact` at `compaction.ts:219`:

```ts
export function shouldCompact(contextTokens, contextWindow, settings) {
    if (!settings.enabled) return false;
    return contextTokens > contextWindow - settings.reserveTokens;
}
```

When the context fills up to `contextWindow - reserveTokens`,
compaction fires *before* the next turn. After compaction, the
next turn has at least `reserveTokens` (16 k default) of room
for the model's response. The agent layer guarantees the
invariant by shrinking the *input*, not by capping the *output*.

## Why this is the right model

Three observations:

1. **The context-window invariant belongs to the upstream.**
   OpenRouter / OpenAI / Anthropic all enforce it server-side
   and know the actual token counts after tokenization. Our
   chars/4 estimator is approximate. Any clamp we compute
   client-side is either too loose (still 400s) or too tight
   (truncates good runs).

2. **Capping `max_tokens` is the wrong knob for the wrong
   problem.** `max_tokens` is a ceiling on the response.
   "Context overflow" is an input-too-big problem. Shrinking the
   input is how you fix it — that's what compaction does.
   Lowering the output ceiling to 50% of the window is a
   coincidence that happens to work on short conversations.

3. **Unbounded `max_tokens` is safe in practice.** Reasoning
   models don't actually produce the full advertised
   `max_completion_tokens` — they produce as much as the
   problem needs and stop. Runaway generation is bounded by
   the model's own training, the user's Ctrl-C, and the
   context-window ceiling. Setting a hard cap here changes
   nothing except the failure mode.

## Where we sit today

Good news: we have the compaction infrastructure.

`crates/anie-config/src/lib.rs:138-142`:
```rust
reserve_tokens: 16_384,
keep_recent_tokens: 20_000,
```

Same defaults as pi. Our `CompactionConfig` shape, `shouldCompact`
equivalent, and compaction loop are already in place. The
context-window invariant is already enforced at the compaction
layer.

What's wrong is one line:

`crates/anie-agent/src/agent_loop.rs:413-419`:
```rust
let options = anie_provider::StreamOptions {
    api_key: request.api_key,
    temperature: None,
    max_tokens: Some(model.max_tokens),   // ← always populated
    thinking: self.config.thinking,
    headers: request.headers,
};
```

We unconditionally forward `model.max_tokens` onto every request.
That's the step pi skips, and that's what causes the 400 when
`model.max_tokens` is a theoretical upstream max.

## Plan

Small, focused change. One agent-loop edit, one downstream cleanup,
one test update.

### PR 1 — Stop forwarding `model.max_tokens` on the main agent path

**Files:**
- `crates/anie-agent/src/agent_loop.rs` — change the `options`
  build to `max_tokens: None`.
- `crates/anie-cli/src/compaction.rs` — verify the compaction
  path still sets `max_tokens` explicitly (it does; no change
  needed, but audit).

**Sites that will keep setting it:**
- Compaction summarization — intentionally bounded.
- Any future scenario that wants a hard cap (none exist today).

**Sites that will stop setting it:**
- The main agent stream (the one that hits every user turn).

### PR 2 — Remove the context-window clamp and the reasoning-default bump

**Files:**
- `crates/anie-provider/src/model.rs::to_model` — revert to:
  ```rust
  max_tokens: self.max_output_tokens.unwrap_or(8_192),
  ```
  No half-window clamp. No reasoning-specific bump. `Model.max_tokens`
  stays in the type for operator reference and for the compaction
  path, but the main path won't send it.
- Drop the clamp tests
  (`to_model_clamps_advertised_max_output_tokens_to_half_of_context_window`,
  `to_model_bumps_default_for_reasoning_models_without_advertised_cap`,
  the existing `to_model_keeps_conservative_default_for_non_reasoning_models`
  which becomes trivial).

**Why now, not later:** the clamp solved a symptom caused by PR
1's problem. Once PR 1 stops sending `max_tokens` on the main
path, the clamp is dead weight — and if some future operator
explicitly uses `Model.max_tokens`, we shouldn't be
second-guessing them.

### PR 3 — Audit / strengthen compaction reserve for reasoning models

**Files:**
- `crates/anie-config/src/lib.rs::CompactionConfig`

**Change:** the default `reserve_tokens = 16_384` matches pi
exactly and is fine for most models. For reasoning models,
16 k can occasionally be tight (the model might emit 8 k of
reasoning + 8 k of visible text). Consider:

Option A: Leave `reserve_tokens` at 16 k and accept the
occasional `ResponseTruncated` (we already route that error
cleanly since commit `42a1268`).

Option B: Allow per-model or per-"is-reasoning" override of
`reserve_tokens`. E.g., double the reserve when the active model
has `supports_reasoning == true`.

**Recommendation: Option A.** Keep `reserve_tokens` as-is for
now; observe whether `ResponseTruncated` fires more often after
PR 1/2 land. If it does, Option B is cheap to add later.

### Test plan

Per PR:

| # | Test | Where |
|---|------|-------|
| 1 | `main_agent_stream_does_not_send_max_tokens` — drive the mock provider with a one-turn run; assert `build_request_body_for_test(...)`'s JSON has no `max_tokens` field. | `crates/anie-integration-tests/tests/provider_replay.rs` |
| 2 | `compaction_summarization_still_sets_max_tokens` — call the compaction path; assert `max_tokens` *is* present in the summarization request. | `crates/anie-cli/src/compaction.rs::tests` |
| 3 | `to_model_preserves_advertised_max_tokens_verbatim_when_provided` — no clamp, no bump. | `crates/anie-provider/src/model.rs::tests` |

### Exit criteria

- [ ] PRs 1 and 2 merged in order.
- [ ] 262 k-context OpenRouter model (the user's original
      failure case) completes a two-turn conversation without a
      400.
- [ ] Existing compaction tests unchanged — the reserve logic is
      untouched.
- [ ] Manual smoke: Nemotron-3-Super free tier (large context,
      reasoning-capable) completes a tool-using multi-turn
      session. No 400s, no `ResponseTruncated` unless the run
      is genuinely hitting the compaction ceiling.

## Risks

- **Some obscure proxy may require `max_tokens`.** Not known
  today. If a future provider does, the compat blob
  (`ModelCompat`) is the right place to surface a per-vendor
  override: e.g.,
  `OpenAICompletionsCompat::requires_max_tokens_default:
  Option<u64>`.

- **Unbounded generation on a buggy model.** Cancellation via
  `Ctrl-C` and the context-window ceiling still apply. Pi has
  shipped this policy for a long time without it being a
  problem.

- **The `model.max_tokens` field becomes near-vestigial.** After
  PR 1+2, it's only read by compaction summarization *as a
  fallback for sizing the summary output*. That's fine; the
  field documents an operator-facing contract even if the
  runtime rarely uses it.

## Reference

- Pi's request builder:
  `pi/packages/ai/src/providers/openai-completions.ts:394-400`
- Pi's compaction settings:
  `pi/packages/coding-agent/src/core/compaction/compaction.ts:115-125`
- Pi's `shouldCompact`:
  `pi/packages/coding-agent/src/core/compaction/compaction.ts:219-222`
- Our current forward site:
  `crates/anie-agent/src/agent_loop.rs:413-419`
- Our current `to_model`:
  `crates/anie-provider/src/model.rs::ModelInfo::to_model`
- Our compaction defaults:
  `crates/anie-config/src/lib.rs:138-142`
- The 400 that started this:
  `Context overflow: ... 262144 in the output ...` (e85a117
  commit message)
