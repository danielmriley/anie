# 01 — Context-aware compaction reserve

## Rationale

`CompactionConfig::reserve_tokens` defaults to 16,384
(`crates/anie-config/src/lib.rs:295-300`) and is treated as a flat
floor regardless of the configured context window:

- `crates/anie-cli/src/controller.rs:937-941` —
  `threshold = context_window.saturating_sub(reserve_tokens)`.

For local models with small windows this is broken:

| `context_window` | `reserve_tokens` | resulting threshold |
|---|---|---|
| 200,000 | 16,384 | 183,616 |
| 65,536 | 16,384 | 49,152 |
| 32,768 | 16,384 | 16,384 |
| 16,384 | 16,384 | **0** |
| 8,192 | 16,384 | **0** (saturated) |

At threshold zero, every turn triggers compaction unconditionally —
expensive, surprising, and not what the user asked for. A configured
8K-context model effectively becomes uncompactable: by the time the
threshold check fires, the request is already over the configured
window.

We need the reserve to scale with the configured window so the
trigger sits in the same relative position regardless of size.

## Design

### Effective reserve formula

```text
effective_reserve(window, configured)
    = min(configured, window / 4)
    .max(MIN_RESERVE_TOKENS)
```

Rationale:

- `min(configured, window / 4)` keeps the configured value as an
  upper bound — explicit user choice still takes precedence — but
  caps it at 25 % of the configured window. That guarantees the
  threshold lives at 75 % of window or higher, so context fills
  before the trigger fires rather than at startup.
- A `MIN_RESERVE_TOKENS` floor (proposed: 1,024) prevents tiny
  windows from producing absurd thresholds (e.g. window 4,096 →
  reserve 1,024 → threshold 3,072).
- `MIN_RESERVE_TOKENS` is also the lower validation bound on
  `[compaction] reserve_tokens` going forward so user-supplied
  values stay sane.

### Where the formula applies

- `Controller::compaction_strategy`
  (`crates/anie-cli/src/controller.rs:887-903`): when constructing
  `CompactionConfig`, pass an `effective_reserve` instead of the
  raw configured value.
- `Session::auto_compact` consumers should keep using the value
  from the supplied `CompactionConfig` — no change needed inside
  `anie-session`.

### Surface area

Keep `CompactionConfig::reserve_tokens` semantically meaning
"effective" (post-clamp). The formula lives at the call site that
builds `CompactionConfig`, not on the config type itself, so
`anie-session` stays unaware of windows. This matches anie's
existing pattern of clamping in the controller layer.

### Optional config key

Expose the floor as a config knob under `[compaction]`:

```toml
[compaction]
reserve_tokens = 16384       # existing
min_reserve_tokens = 1024    # new, optional
```

Defaulted to 1,024 if absent. Mainly an escape hatch for very
unusual setups; not surfaced in the default template.

## Files to touch

- `crates/anie-config/src/lib.rs`
  - Add `min_reserve_tokens` to `CompactionConfig` with default 1,024.
  - Add validation (`>= 256` or similar) in `PartialCompactionConfig`
    merge to keep absurd values out.
  - Update `default_config_template()` only if we choose to surface
    the new knob. Recommended: leave it commented out by default.
- `crates/anie-cli/src/controller.rs`
  - In `compaction_strategy`, compute `effective_reserve(window,
    configured, min_reserve)` and use that in the
    `CompactionConfig` it returns.
- `crates/anie-cli/src/runtime/state_summary.rs` (or whatever the
  `/state` summary file is)
  - Surface the *effective* reserve in the summary so users can
    see the clamp at work.

## Phased PRs

### PR A — Add `effective_reserve` helper + apply at call site

**Change:**

- New free function `effective_reserve(window, configured,
  min_reserve) -> u64` in `anie-cli` (or in `anie-session` if
  more callers ever appear).
- `Controller::compaction_strategy` uses it.

**Tests:**

- `effective_reserve_keeps_configured_when_under_quarter_window`
- `effective_reserve_clamps_to_quarter_window`
- `effective_reserve_floors_at_min_reserve`
- Existing compaction integration tests must still pass for
  large-window cases (no regression).

**Exit criteria:**

- 200K-window default behavior unchanged (configured 16,384 ≤
  50,000 = 200,000 / 4 → still 16,384).
- 16K-window default behavior changes from "compact every turn"
  to "compact at ~12K tokens" (16,384 - 4,096 = 12,288 threshold).
- 8K-window default behavior: threshold is 6,144 instead of 0.

### PR B — Optional `min_reserve_tokens` config knob

**Change:**

- Add `min_reserve_tokens: u64` to `CompactionConfig` and
  `PartialCompactionConfig` with default 1,024.
- Plumb into `effective_reserve`.

**Tests:**

- `partial_compaction_config_loads_min_reserve_tokens`
- `effective_reserve_honors_explicit_min_floor`

**Exit criteria:**

- Setting `min_reserve_tokens = 2048` raises the 8K-window
  threshold from 6,144 to 6,144 (no change — the quarter-window
  cap was already 2,048; this is a no-op for that case but matters
  for explicit user-set higher floors).

### PR C — `/state` and load-failure messages reflect effective values

**Change:**

- Update the `/state` summary message to show
  `reserve_tokens (effective: N)` when the effective value
  differs from the configured one.
- Optionally include the same in `ProviderError::ModelLoadResources`
  formatting if we want to point users at why their threshold
  is so low on small windows.

**Tests:**

- `state_summary_shows_effective_reserve_when_clamped`
- `state_summary_shows_only_configured_when_no_clamp_applies`

## Test plan

Beyond the per-PR lists:

- Property test (cheap version): for `window` in {2048, 4096, 8192,
  16384, 32768, 65536, 131072, 200000} and `configured` in {1024,
  4096, 16384, 65536}, assert
  `effective_reserve(...) <= window` and the resulting
  `threshold = window - effective_reserve` is non-zero (unless
  window itself is below `min_reserve_tokens`, which is a
  configuration error).

## Risks

- **Behavior change for users running small-window models today.**
  Currently they likely see compaction every turn. After this PR
  they see compaction only when context actually fills. That's
  an improvement, not a regression, but the changed cadence is
  user-visible.
- **Configured `reserve_tokens > window/4` becomes a soft cap.** A
  user who explicitly set `reserve_tokens = 24576` on a 32K window
  will see effective reserve clamp to 8,192. Document this in the
  config template comment and surface in `/state`.

## Exit criteria

- [ ] Default behavior on small windows is no longer "always
      compact"; threshold lives at ~75 % of window.
- [ ] User-set high values are visibly clamped (not silently).
- [ ] Existing 32K+ tests unchanged.
- [ ] `cargo test --workspace`, clippy clean.

## Deferred

- **Adaptive `keep_recent_tokens`** along the same lines.
  `keep_recent_tokens = 20_000` on a 16K window is also wrong, but
  the relationship is more nuanced (we want to keep enough history
  for the model to be useful). Plan separately if it becomes a
  problem.
- **Auto-discovery of context window from Ollama.** Currently the
  user sets it; we could query `/api/show`. Out of scope here.
