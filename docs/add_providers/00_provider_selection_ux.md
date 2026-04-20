# Plan 00 — Provider selection UX

Shared prerequisite for every provider plan in this folder. Makes
the onboarding and `/providers` flows scale past five providers
without the UI getting worse with each addition.

## Motivation

Today's onboarding flow and `/providers` overlay have the current
provider set (`anthropic`, `openai`, local) wired up more or less
by hand. The overlay's table shape works fine at three rows; at
ten rows (OpenRouter + xAI + Groq + Cerebras + Mistral + Gemini +
Azure + Bedrock + current set) it won't.

Two concrete problems surface with the existing UX:

1. **Onboarding doesn't offer new providers by default.** The
   first-run flow mentions Anthropic, OpenAI, and an Ollama probe.
   Adding a row per new provider would turn the first-run screen
   into a grid search.
2. **`/providers` table has no category grouping or search.**
   Currently you scroll a flat list. Ten rows is fine; twenty is
   not, especially with similar-looking OpenAI-compatible entries
   (`xAI` / `Groq` / `Cerebras` / `Mistral` / `OpenRouter` all
   look alike at a glance).

This plan lands the reusable UX pieces before any per-provider
plan needs them.

## Goals

1. **Category-aware provider picker.** A single widget that lists
   providers grouped by `ProviderType` (or a new
   `ProviderCategory` enum if richer grouping is warranted), with
   a search field at top.
2. **Preset catalog.** A new `anie-providers-builtin` module
   (`provider_presets.rs`) that holds the display name, default
   base URL, API-kind hint, and auth metadata for every built-in
   provider. Onboarding and `/providers` both read from it. Adding
   a new provider becomes a one-row addition.
3. **Onboarding short-list.** First-run shows the three or four
   "most likely to work out of the box" (Anthropic, OpenAI,
   OpenRouter, plus Ollama probe). A "more providers…" option
   leads to the full category picker.

No behavior change on already-configured providers. No config
migration required — the preset catalog is display-only; actual
provider config still lives in `config.toml` and `auth.json`.

## Files to change

| File | Change |
|---|---|
| `crates/anie-providers-builtin/src/provider_presets.rs` (new) | `ProviderPreset` struct, a static `BUILTIN_PRESETS: &[ProviderPreset]`, and a `find_preset(name: &str)` helper. |
| `crates/anie-providers-builtin/src/lib.rs` | `pub use provider_presets::{ProviderPreset, builtin_presets}`. |
| `crates/anie-tui/src/overlays/onboarding.rs` | Replace the hand-coded provider list with `builtin_presets()`. Add a "more providers" entry that opens the category picker. |
| `crates/anie-tui/src/overlays/providers.rs` | Add an "Add provider…" action that opens the category picker. Category picker wraps `SelectList<ProviderPreset>` from plan 12's widget. |
| `crates/anie-tui/src/overlays/mod.rs` | Export the new shared category-picker widget if we extract one. |

## Design sketch

### `ProviderPreset`

```rust
/// A built-in provider preset that the onboarding flow and the
/// /providers overlay present as a ready-to-configure option.
/// Display-only — actual provider configuration still lives in
/// `config.toml` and `auth.json`.
pub struct ProviderPreset {
    /// Canonical provider name used as the config key.
    pub name: &'static str,
    /// Human-readable label for the UI.
    pub display_name: &'static str,
    /// Which pre-existing ApiKind this preset targets. Presets
    /// never invent a new ApiKind — new ones come via a dedicated
    /// provider module and plan.
    pub api_kind: ApiKind,
    /// Default base URL. Can be overridden by the user in
    /// `/providers`.
    pub base_url: &'static str,
    /// What kind of auth this preset expects.
    pub auth_hint: AuthHint,
    /// Category used by the picker's grouping header.
    pub category: ProviderCategory,
    /// One-line description shown in the picker.
    pub tagline: &'static str,
    /// Optional URL where the user can obtain an API key. Used
    /// by the onboarding flow to offer a clipboard / browser
    /// shortcut.
    pub api_key_source_url: Option<&'static str>,
}

pub enum AuthHint {
    ApiKey { env_var: Option<&'static str> },
    Local,                          // no auth
    OAuth,                          // reserved; future use
    Cloud { credentials: &'static str }, // AWS/Azure
}

pub enum ProviderCategory {
    /// Frontier hosted (Anthropic, OpenAI, Google, Bedrock…).
    Frontier,
    /// OpenAI-compatible aggregators and fast inference (OpenRouter, xAI, Groq, Cerebras, Mistral).
    OpenAICompatible,
    /// Enterprise cloud (Azure OpenAI).
    Cloud,
    /// Local servers (Ollama, LM Studio, vLLM).
    Local,
}
```

### Category picker widget

Backed by `crates/anie-tui/src/widgets/select_list.rs::SelectList`
(shipped in plan 12). Two modes:

- **Search-open**: a `TextField` at the top filters the list by
  substring match on display name + tagline.
- **Grouped view**: when search is empty, show category headers
  with the presets below each.

Return type: `Option<ProviderPreset>`. The onboarding and
`/providers` call sites continue from there with the preset's
fields.

### Onboarding wiring

Current flow (simplified):

```
Welcome → [Anthropic] [OpenAI] [Ollama] [Skip]
```

Becomes:

```
Welcome → [Anthropic] [OpenAI] [OpenRouter] [Ollama] [More providers…] [Skip]
```

Where "More providers…" opens the category picker. The four
always-visible shortcuts are the ones new users are most likely
to have a key for; the picker behind "More providers" holds the
full set and grows automatically as each subsequent plan lands.

### `/providers` wiring

Current overlay: table of configured providers with add/edit/remove
actions. The "Add provider" action today prompts for a free-form
name + base URL. After this plan, "Add provider" shows the
category picker first; picking a preset pre-fills name + base URL
+ API kind; the user just pastes the key.

Custom / unknown providers are still supported — the picker has
a "Custom…" entry that opens today's free-form form.

## Test plan

| # | Test |
|---|---|
| 1 | `builtin_presets_include_every_shipped_provider` — walk `builtin_presets()` and assert Anthropic + OpenAI + local are present with correct ApiKind. |
| 2 | `preset_category_picker_filters_by_display_name_substring` — drive the search field, assert the visible set narrows. |
| 3 | `preset_category_picker_groups_by_category_when_search_empty` — no filter → grouped. |
| 4 | `onboarding_shortlist_renders_first_four_presets` — snapshot test on the shortlist. |
| 5 | `providers_add_uses_preset_to_prefill` — pick a preset, assert the add-form opens with correct defaults. |
| 6 | `custom_entry_still_opens_freeform_form` — pick "Custom", assert the existing free-form form appears. |

## Exit criteria

- [ ] `ProviderPreset` catalog lives in `anie-providers-builtin`
      and mirrors every entry the current onboarding + `/providers`
      hard-codes.
- [ ] Onboarding uses the preset catalog; first-run shortlist +
      "More providers" exist.
- [ ] `/providers` add flow uses the category picker with a
      Custom fallback.
- [ ] Adding a new provider to the catalog in any future plan
      requires one row in the preset list and no TUI changes.

## Out of scope

- Any actual provider implementations (those are the per-provider
  plans 01–06).
- OAuth / browser-launch auth flows (deferred; `AuthHint::OAuth`
  variant is reserved for them).
- Per-preset default-model autofill (nice-to-have; each provider
  plan can add its catalog entry without this).
