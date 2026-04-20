# Milestone 2 — OpenRouter

Two PRs. First adds the preset + catalog entries + static
routing-preference plumbing. Second adds live model discovery so
users can access the long tail beyond the curated eight.

Spec reference: [`../01_openrouter.md`](../01_openrouter.md).

## Dependencies

- Milestone 0 (Foundation). Uses `ModelCompat::OpenAICompletions`
  with `openrouter_routing`, and
  `ThinkingRequestMode::NestedReasoning`.
- Milestone 1 (UX prerequisite). Adds the preset to
  `BUILTIN_PRESETS` and the shortlist.

---

## PR A — OpenRouter preset + eight-model catalog + wiring

**Goal:** Users can add an OpenRouter API key via onboarding and
run a two-turn conversation on one of the eight curated models.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs`
- `crates/anie-providers-builtin/src/models.rs`
- `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`
  (if the nested-reasoning-mode branching needs one more hook —
  otherwise just catalog data changes)
- `crates/anie-tui/src/overlays/onboarding.rs` (append to
  shortlist)

### Steps

1. Add the `openrouter` preset entry matching the spec's
   "Onboarding integration" section:
   - `category: OpenAICompatible`
   - `base_url: https://openrouter.ai/api/v1`
   - `env_var: Some("OPENROUTER_API_KEY")`
   - `api_key_source_url: Some("https://openrouter.ai/keys")`
2. Add the eight curated catalog entries from the spec's
   "Model catalog entries" table. Anthropic-routed entries get
   `replay_capabilities` with `requires_thinking_signature:
   true`. Reasoning-capable entries get
   `reasoning_capabilities.request_mode = NestedReasoning` (and
   `control = Native`).
3. Extend the onboarding shortlist to include OpenRouter.
4. Tests per the spec's Test plan #1–#3 and #5.

### Test plan

| # | Test |
|---|---|
| 1 | `openrouter_preset_registered` |
| 2 | `openrouter_request_uses_nested_reasoning_object` — assert the outbound body for a reasoning-capable OpenRouter model has `reasoning.effort` and no `reasoning_effort`. |
| 3 | `openrouter_routing_preferences_propagate_when_configured` — set `openrouter_routing = Some({ order: ["anthropic"], ..default })`, assert the `provider` field in the outbound body. |
| 4 | `openrouter_model_ids_preserve_provider_prefix` |
| 5 | Invariant suite: extend `provider_replay.rs` so OpenRouter appears in every cross-provider invariant. |
| 6 | Onboarding shortlist snapshot includes OpenRouter. |

### Exit criteria

- [ ] Preset registered and reachable from
      `BUILTIN_PRESETS`.
- [ ] Eight catalog entries present with pricing fields zeroed
      and a dated catalog-comment.
- [ ] Tests 1–6 pass.
- [ ] Onboarding shortlist includes OpenRouter (from milestone
      1's slot).

---

## PR B — Live model discovery via `/api/v1/models`

**Goal:** Users who want models beyond the eight curated ones can
refresh from the OpenRouter `/api/v1/models` endpoint in the
`/providers` overlay without hand-editing config.

### Files
- `crates/anie-providers-builtin/src/model_discovery.rs` (add
  OpenRouter-specific fetch + parse)
- `crates/anie-tui/src/overlays/providers.rs` (wire the refresh
  action to OpenRouter's discovery URL when the user-configured
  base_url matches)

### Steps

1. Inspect the OpenAI-style `/models` parser in
   `model_discovery.rs`. OpenRouter's payload is close but has
   differences (`context_length` vs `context_window`,
   `pricing.prompt` vs per-token cost). Add a small adapter.
2. Wire `/providers` → "Refresh models" action for the OpenRouter
   entry. The action already exists for OpenAI; the wiring is
   URL-conditional.
3. Fixture test with a saved OpenRouter `/models` response.

### Test plan

| # | Test |
|---|---|
| 7 | `openrouter_model_discovery_parses_models_endpoint` (from spec Test plan #4) |
| 8 | `openrouter_discovery_populates_context_window_from_context_length_field` |
| 9 | Manual smoke (per spec) — documented in PR description. |

### Exit criteria

- [ ] `/providers` → Refresh on OpenRouter fetches the live
      model list.
- [ ] Discovered entries show up in the `/model` picker.
- [ ] Tests 7–8 pass.

---

## Milestone exit criteria

- [ ] Both PRs merged.
- [ ] Eight starter models available without discovery.
- [ ] Live discovery fetches the full catalog on demand.
- [ ] Invariant suite exercises at least one OpenRouter model
      on every cross-provider invariant.
- [ ] Manual smoke: two-turn conversation on
      `anthropic/claude-sonnet-4.6` with thinking level `high`
      completes without replay errors.
