# Plan 06 — provider streaming + local models

**Findings covered:** #15, #17, #18, #49, #53, #54, #55

This plan groups the provider-side cleanup where runtime cost is paid
per request, per discovered model, or per streaming fragment.

## Rationale

The review surfaced three related provider themes:

1. **Request-body duplication with correctness constraints**
   (Anthropic thinking config, **#15**)
2. **Streaming fragment allocation paths**
   (Anthropic/OpenAI text deltas, tagged reasoning splitter,
   **#17, #49, #54**)
3. **Repeated work in model discovery / local probe helpers**
   (**#18, #53, #55**)

The pi comparison is important here mostly as a **non-adoption**
result: pi does not have anie's richer local reasoning /
tagged-reasoning machinery, so these fixes should preserve anie's
current model rather than chasing pi's simpler shape.

## Design

### 1. Fix Anthropic request building without breaking semantics

The review correction matters: the second `thinking_config` use is
there to re-assert `temperature = 1.0` after any user temperature
override. So the fix is:

1. compute `thinking_config` once
2. preserve the final insert ordering
3. do **not** delete the second semantic effect

This should be treated as a correctness-sensitive perf cleanup, not a
micro-optimization.

### 2. Make streaming deltas skip empty fragments

Apply the same pattern to both streaming providers:

- borrow `&str`
- skip empty fragments
- allocate once when pushing the event payload

This addresses Anthropic and OpenAI/OpenRouter together.

### 3. Rewrite tagged-reasoning prefix extraction

`String::drain(..).collect::<String>()` in the tagged-reasoning
splitter should become a move-based split using `split_off` +
`mem::replace`, because the delimiter index is already a byte offset
on ASCII tag characters.

### 4. Normalize invariant local-discovery values once per probe

Within a single `/v1/models` response:

- trimmed base URL is invariant
- `.../v1` base URL is invariant
- provider/base URL lowercase forms are invariant

Hoist them out of the per-model loop and thread them into a
normalized helper for local reasoning capability defaults.

### 5. Make model-discovery cache hits cheap

The model-discovery cache should return shared ownership on hits:

- `Arc<Vec<ModelInfo>>` or `Arc<[ModelInfo]>`

Whichever fits the current call sites more cleanly is fine, as long
as cache hits stop deep-cloning the full discovered model list.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/anthropic.rs` | single-compute thinking config; delta fast path |
| `crates/anie-providers-builtin/src/openai/streaming.rs` | delta fast path |
| `crates/anie-providers-builtin/src/openai/tagged_reasoning.rs` | move-based prefix extraction |
| `crates/anie-providers-builtin/src/model_discovery.rs` | shared cache-hit ownership |
| `crates/anie-providers-builtin/src/local.rs` | hoisted normalized probe inputs |
| `crates/anie-providers-builtin/src/util.rs` | lowercase body once in error classifier |

## Phased PRs

### PR A — Anthropic request-body ordering

1. Bind `thinking_config` once.
2. Keep the final `temperature = 1.0` re-assertion ordering intact.

### PR B — Anthropic empty-delta cleanup

1. Skip empty text deltas in `anthropic.rs`.
2. Keep this separate from the request-body ordering fix.

### PR C — OpenAI empty-delta cleanup

1. Skip empty text deltas in `openai/streaming.rs`.

### PR D — tagged reasoning splitter cleanup

1. Replace `drain(..).collect::<String>()` in the tagged splitter.
2. Add multi-fragment regression tests.

### PR E — model-discovery cache ownership

1. Make cache hits shared-ownership based.

### PR F — local probe normalization

1. Hoist invariant base-URL/provider normalization out of the
   per-model loop.
2. Keep discovered model shapes unchanged.

### PR G — provider helper sweep

1. Lowercase HTTP error bodies once.
2. Keep this separate and tiny.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `anthropic_thinking_request_reasserts_temperature_after_user_override` | `anthropic.rs` tests |
| 2 | `anthropic_empty_text_delta_is_ignored` | same |
| 3 | `openai_empty_text_delta_is_ignored` | `openai/streaming.rs` tests |
| 4 | `tagged_reasoning_splitter_handles_multiple_fragments_without_regression` | `tagged_reasoning.rs` tests |
| 5 | `model_discovery_cache_hit_reuses_shared_model_list` | `model_discovery.rs` tests |
| 6 | `local_probe_hoisted_base_url_still_emits_same_model_shape` | `local.rs` tests |
| 7 | `classify_http_error_context_overflow_detection_still_works` | `util.rs` tests |

## Risks

- **Anthropic semantics regression:** this is the most sensitive
  change in the plan; tests must protect the temperature/thinking
  contract.
- **Shared ownership API churn:** if cache consumers expect owned
  vectors, introduce the `Arc` at the boundary carefully.
- **Tagged reasoning edge cases:** chunk boundaries can be tricky;
  keep the existing tests and add more.

## Exit criteria

- [ ] Anthropic request-building computes thinking config once while
      preserving required ordering.
- [ ] Empty streaming deltas are skipped in both provider families.
- [ ] Tagged reasoning no longer does `drain(..).collect::<String>()`.
- [ ] Model-discovery cache hits no longer deep-clone the discovered
      model list.
- [ ] Local probe no longer recomputes invariant normalization in the
      per-model loop.

## Deferred

- Any redesign of `ReasoningCapabilities` or local reasoning policy.
- Any cross-provider abstraction beyond the targeted cleanup above.
