# Milestone 1 — Provider selection UX

Ships the shared UX infrastructure that every subsequent
provider plan lands into — the `ProviderPreset` catalog, a
category-aware picker, and wiring into both onboarding and
the `/providers` overlay.

Spec reference: [`../00_provider_selection_ux.md`](../00_provider_selection_ux.md).

## Dependencies

- Milestone 0 (Foundation). The preset catalog references
  `ApiKind` and can attach `ModelCompat` defaults per preset.

## PR split

Two PRs. The first lands the data type + catalog + backend
wiring with existing onboarding behavior unchanged. The
second flips onboarding and `/providers` over to the picker.

Keeping the switch in its own PR lets us revert the UI change
if it looks wrong under real-terminal rendering without
losing the data work.

---

## PR A — `ProviderPreset` catalog + category picker widget

**Goal:** Shipping the data types and the search-first picker
widget, without any caller using them yet.

### Files
- `crates/anie-providers-builtin/src/provider_presets.rs` (new)
- `crates/anie-providers-builtin/src/lib.rs` (re-exports)
- `crates/anie-tui/src/widgets/category_picker.rs` (new) — or
  reuse the existing `SelectList` widget from plan 12 if the
  grouping headers can be inlined as list items.
- `crates/anie-tui/src/widgets/mod.rs` (export)

### Steps

1. Implement `ProviderPreset`, `AuthHint`, `ProviderCategory`
   per the spec's "Design sketch" section.
2. Populate `BUILTIN_PRESETS` with exactly the three
   providers that ship today (Anthropic, OpenAI, local). No
   new presets yet — those come in per-plan PRs.
3. Category picker: wrap `SelectList<ProviderPreset>` with a
   `TextField` filter. When the filter is empty, render
   category headers inline; when the user types, collapse to
   a flat ranked list.
4. Unit tests for the catalog round-trip and picker filter
   behavior. No TUI snapshot tests required here — the widget
   is exercised by its callers in PR B.

### Test plan

| # | Test |
|---|---|
| 1 | `builtin_presets_include_anthropic_openai_local` |
| 2 | `category_picker_groups_by_category_when_filter_empty` |
| 3 | `category_picker_ranks_by_substring_when_filter_set` |
| 4 | `category_picker_custom_entry_always_present` — the "Custom…" fallback row is always last. |

### Exit criteria

- [ ] `ProviderPreset` and friends exported from
      `anie-providers-builtin`.
- [ ] Category picker widget compiles and unit-tests green.
- [ ] No caller wired yet — existing onboarding / overlays
      untouched.

---

## PR B — Wire onboarding + `/providers` to the picker

**Goal:** Swap both call sites over to the preset-driven picker.
Preserves existing behavior for already-configured providers.

### Files
- `crates/anie-tui/src/overlays/onboarding.rs`
- `crates/anie-tui/src/overlays/providers.rs`

### Steps

1. **Onboarding shortlist.** First-run screen shows four
   always-visible presets (Anthropic, OpenAI, OpenRouter if
   the plan 01 preset has landed, Ollama) and a "More
   providers…" entry that opens the category picker. Since
   OpenRouter doesn't exist in the preset catalog yet at
   milestone 1, the shortlist ships with three presets until
   milestone 2 appends the fourth.
2. **`/providers` Add flow.** Replace the free-form first-
   screen prompt with the category picker; a "Custom…" entry
   opens the existing free-form form as-is.
3. **Form prefill.** When a preset is picked, pre-fill the
   existing add-provider form with `base_url`, `api_kind`, and
   `env_var`. User only has to paste the API key.

### Test plan

| # | Test |
|---|---|
| 5 | `onboarding_shortlist_renders_first_three_presets` — snapshot |
| 6 | `onboarding_more_providers_opens_category_picker` |
| 7 | `providers_add_preset_prefills_form_fields` |
| 8 | `providers_add_custom_opens_existing_freeform_form` |
| 9 | Existing onboarding + `/providers` tests pass unchanged. |

### Exit criteria

- [ ] Onboarding first-run shortlist + "More providers…".
- [ ] `/providers` Add action uses the picker.
- [ ] Every existing onboarding/providers test stays green.
- [ ] A user who already has providers configured sees no
      change in the table view.

---

## Milestone exit criteria

- [ ] Both PRs merged.
- [ ] Onboarding and `/providers` both read from
      `BUILTIN_PRESETS`.
- [ ] Adding a new provider in any subsequent plan is a one-row
      addition to the preset catalog; no TUI changes.
- [ ] Existing users don't see any behavior change on
      already-configured providers.
