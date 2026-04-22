# Plan 08 — low-risk helper sweep

**Findings covered:** #14, #22, #28, #29, #30, #40, #41, #42, #50, #57

This is intentionally the last plan. It collects the remaining
cleanup work that is real but not urgent enough to compete with the
hot paths.

## Rationale

After plans 01-07 land, the report still leaves a tail of cheaper
helper-only inefficiencies:

- token-estimation and text-assembly helpers (**#14, #30, #50, #57**)
- catalog lookup / dedupe cleanups (**#28, #29**)
- small TUI/CLI helper allocations (**#22, #40, #41, #42**)

These should be fixed, but not mixed into the hotter-path PRs where
they would only add review noise.

## Design

### 1. Group by module family, not by severity

Keep each PR coherent even if all the findings are low-risk:

- text assembly helpers together
- catalog/model helper cleanups together
- TUI/CLI helper cleanups together

### 2. Prefer direct-buffer rewrites over "clever" abstractions

Most of these are simple:

- replace collect-then-join with direct `String` building
- replace repeated scans with one pass
- replace cheap repeated allocations with direct construction

No new shared framework is needed.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | token-estimation helper cleanup (#14) if not already absorbed elsewhere |
| `crates/anie-providers-builtin/src/openai/convert.rs` | text assembly cleanup (#30) |
| `crates/anie-cli/src/compaction.rs` | text assembly cleanup (#50) |
| `crates/anie-cli/src/print_mode.rs` | text assembly cleanup (#57) |
| `crates/anie-cli/src/model_catalog.rs` | lookup/dedupe cleanup (#28, #29) |
| `crates/anie-tui/src/output.rs` / `app.rs` / `input.rs` | small helper allocations (#22, #41, #42) if not already absorbed earlier |

## Phased PRs

### PR A — text assembly helper sweep

1. Fix collect-then-join helpers in:
   - `openai/convert.rs`
   - `compaction.rs`
   - `print_mode.rs`

### PR B — token-estimation helper cleanup

1. Land #14 separately if it is still open after the session/tool
   plans settle.
2. Keep it small; do not mix it into the catalog PR unless tests
   overlap naturally.

### PR C — model-catalog helper cleanup

1. Remove redundant multi-scan lookup in `resolve_requested_model`.
2. Remove clone-heavy dedupe key construction in `dedupe_models`.
3. Keep behavior and output ordering unchanged.

### PR D — remaining TUI/CLI helper allocations

1. `prefix.to_string()` cleanup in `output.rs` if not already landed.
2. path-part / helper allocation cleanups in `app.rs`
3. small `submit()` helper cleanup in `input.rs` if still open

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | text assembly helpers preserve exact string output | module-local tests |
| 2 | `resolve_requested_model` behavior remains identical for provider-qualified and bare IDs | `model_catalog.rs` tests |
| 3 | `dedupe_models` still keeps the later duplicate entry | same |
| 4 | small TUI/CLI helper cleanups preserve existing snapshots / command parsing | relevant tests |

## Risks

- **Review noise:** the main risk is bundling unrelated low-risk items
  together. Keep PR boundaries clean.
- **String output drift:** these helpers are easy to "optimize" while
  accidentally changing separators or formatting.

## Exit criteria

- [ ] Remaining collect-then-join helpers in the reviewed files are
      gone.
- [ ] `model_catalog.rs` lookup/dedupe helpers are simplified without
      behavior drift.
- [ ] The last small helper allocations from the report are either
      fixed here or explicitly deferred in the execution tracker.

## Deferred

- Any finding that turns out to require non-trivial behavior changes
  should be split back out of this sweep and given its own plan or
  landed as part of a hotter-path plan instead.
