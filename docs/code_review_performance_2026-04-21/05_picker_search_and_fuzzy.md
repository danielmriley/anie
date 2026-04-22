# Plan 05 — picker search + fuzzy matching

**Findings covered:** #43, #44, #45, #46, #51

This plan focuses on the per-keystroke allocation paths in model
picker search, slash-command autocomplete, and the shared fuzzy
matching helper.

## Rationale

The report found two layers of work firing on every keystroke:

1. the fuzzy scorer lowercases the query on every call (**#51**)
2. autocomplete lowercases command names / argument values for every
   candidate on every keypress (**#43, #44**)

It also found two smaller text-field helpers worth cleaning up once
the hot search path is fixed:

- `cursor_x` clones a prefix just to count chars (**#45**)
- `render_value` clones the full string in the unmasked case (**#46**)

The pi comparison adds one structural idea worth copying:

- tokenized fuzzy filtering + swapped-digit fallback
  (`pi/packages/tui/src/fuzzy.ts:71-133`)

## Design

### 1. Split the scorer into "already lowered query" and wrapper APIs

Keep the current ranking rules. Add a lower-level API that accepts a
pre-lowercased query:

```rust
pub(crate) fn fuzzy_score_lowered(query_lower: &str, candidate: &str) -> Option<u32>
```

The old `fuzzy_score(query, candidate)` can remain as a convenience
wrapper if tests or non-hot callers still want it.

### 2. Add tokenized matching to the model picker

Do **not** replace anie's tiered scoring with pi's simpler one. Add
only the useful structural ideas:

1. split the query on whitespace
2. require all tokens to match
3. combine token scores
4. optionally add the swapped-digit fallback from pi for versioned
   model names

This gives better picker ergonomics without sacrificing anie's
current ranking logic.

### 3. Precompute lowercase command metadata

Slash-command and static-argument matching should not lower-case the
same stable strings on every keypress. Add cached lowercase forms
where the metadata is built:

- command name lowercase
- enumerated value lowercase
- subcommand lowercase

If a source is dynamic, lower-case it once per refresh, not once per
comparison.

### 4. Clean up the text-field helpers last

After the search path is fixed:

- make `cursor_x` count directly
- consider `Cow<'_, str>` for `render_value` if callers can accept it

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/src/widgets/fuzzy.rs` | lowercased-query scorer + tokenized matching support |
| `crates/anie-tui/src/overlays/model_picker.rs` | compute lowered search once, use tokenized scoring |
| `crates/anie-tui/src/autocomplete/command.rs` | lowercased command/argument metadata and lookup cleanup |
| `crates/anie-tui/src/widgets/text_field.rs` | `cursor_x` and `render_value` cleanup |

## Phased PRs

### PR A — lowered-query fuzzy scorer API

1. Add `fuzzy_score_lowered`.
2. Keep the existing ranking logic unchanged.
3. Leave tokenization for a separate PR.

### PR B — tokenized model-picker filtering

1. Compute `search_lower` once per filter pass.
2. Add tokenized matching in the model picker.
3. Add swapped-digit fallback if it does not destabilize ranking.

### PR C — command + argument autocomplete caches

1. Store lowercase command names at registration/build time.
2. Store lowercase argument values for static enumerations.
3. Remove repeated `.to_lowercase()` inside hot filters.

### PR D — text-field helper cleanup

1. `cursor_x` counts directly.
2. `render_value` returns borrowed data in the unmasked case if the
   call sites support it; otherwise keep it as a low-risk clone and
   mark #46 deferred.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `fuzzy_score_lowered_matches_existing_single_token_rankings` | `widgets/fuzzy.rs` tests |
| 2 | `tokenized_query_requires_all_tokens` | same |
| 3 | `digit_swap_query_matches_expected_model_variants` | same / model picker tests |
| 4 | `command_name_suggestions_no_longer_depend_on_per-candidate_lowercasing` | `autocomplete/command.rs` tests |
| 5 | `cursor_x_matches_masked_and_unmasked_char_count` | `widgets/text_field.rs` tests |

## Risks

- **Ranking churn:** tokenization and swapped-digit support can move
  results around. Tests should lock down the intended behavior.
- **Metadata duplication:** lowercase caches must stay in sync with
  the displayed strings.
- **Over-generalizing dynamic sources:** keep the caching story
  simple for static metadata first.

## Exit criteria

- [ ] Query lowercase is computed once per search/filter pass, not on
      every scorer call.
- [ ] Model picker supports tokenized matching.
- [ ] Command / argument autocomplete no longer lowercases every
      candidate per keypress.
- [ ] `cursor_x` no longer clones the string prefix just to count
      chars.

## Deferred

- Any broader redesign of picker ranking beyond tokenization and
  swapped-digit fallback.
- `render_value` API churn if it turns out not to be worth changing
  for the first pass.
