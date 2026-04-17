# Phase 6 — Polish, Docs, and Validation

This phase hardens the complete dynamic model-menu feature set for release. No new features — only polish, testing, documentation, and validation.

## Why this phase exists

By Phase 5, all the pieces work end-to-end. This phase ensures they feel **coherent and professional** across every entry point and edge case.

---

## Polish targets

### 1. Picker visual quality

Verify across all launch points:

- `/model` (input-replacement picker)
- onboarding (in-card picker)
- provider management "View Models" (in-popup picker)

Checklist:

- [ ] border style matches the rest of the TUI (same colors as output pane tool boxes)
- [ ] search input has visible cursor
- [ ] highlighted row uses the same accent color as the rest of the UI
- [ ] current-model `✓` marker is visible and correctly positioned
- [ ] provider badges `[provider]` are dimmed/muted, not dominant
- [ ] loading spinner uses the same `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` frames as the rest of the TUI
- [ ] error messages are red and actionable (not raw error types)
- [ ] scroll indicator `(N/M)` is visible when list exceeds visible height
- [ ] footer hints match the dimmed-gray footer style used elsewhere

### 2. Search UX

- [ ] focus lands on search input immediately on picker open
- [ ] prefilled search text is visible and editable
- [ ] empty search shows all models
- [ ] no-results state shows a clear "No matching models" message
- [ ] clearing search restores the full list
- [ ] selection position resets sanely when filter results change

### 3. Refresh UX

- [ ] `r` key shows loading state with spinner
- [ ] successful refresh updates the list and preserves search text
- [ ] failed refresh shows inline error, does not close the picker
- [ ] cache behavior is correct: first open is fast (cached), `r` fetches fresh data

### 4. Status messaging

After model selection from any path:

- [ ] system/status message shows `"Model: {model_id}"` (not a multi-step success screen)
- [ ] status bar updates immediately with new provider/model
- [ ] when config is written, message includes the file path

### 5. Edge cases

- [ ] picker with 0 models (no local server, no API key) shows helpful empty state
- [ ] picker with 1 model auto-selects or shows the single item
- [ ] picker with 100+ models scrolls correctly
- [ ] very long model IDs truncate without breaking layout
- [ ] very narrow terminal (40 columns) still renders usably
- [ ] very short terminal (12 rows) — picker height degrades gracefully

---

## Documentation updates

### README.md

Update the following sections:

- **Highlights**: mention dynamic model selection
- **Quick start / first run**: note that onboarding now shows available models
- **Slash commands**: update `/model` description:
  ```
  /model [query]  — Open model picker, or switch immediately if query is an exact match
  ```
- **CLI Reference**: add `anie models` command:
  ```
  anie models [--provider <name>] [--refresh]   List available models
  ```
- **Keyboard shortcuts**: list `Ctrl+O` for model picker

### docs/arch/onboarding_flow.md

Update to describe the new model-picker step in each provider path.

### docs/onboarding-and-keyring.md

Update implementation checklist to reflect the new model selection capabilities.

---

## End-to-end validation matrix

### `/model` interactive command

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `/model` with no args, provider has models | picker opens in input pane |
| 2 | `/model gpt-4o` exact match | instant switch, status message |
| 3 | `/model gpt` partial, no exact match | picker opens with "gpt" prefilled |
| 4 | `/model` while agent is streaming | "cannot open" message, no picker |
| 5 | `Ctrl+O` while idle | picker opens |
| 6 | select model from picker | editor restored, status bar updated |
| 7 | cancel picker (Esc) | editor restored, no change |
| 8 | refresh from picker (`r`) | loading → updated list |
| 9 | editor had draft text before `/model` | draft preserved after cancel |

### Onboarding

| # | Scenario | Expected |
|---|----------|----------|
| 1 | local server → select server → model picker | picker inside card |
| 2 | API preset → enter key → validate → model picker | picker inside card |
| 3 | custom endpoint → validate → model picker | picker inside card |
| 4 | back from picker → previous state preserved | keys/forms/selection intact |
| 5 | refresh in picker after local model change | new model appears |
| 6 | discovery failure → error with fallback/retry | graceful error state |
| 7 | select model → Done → config written | correct provider + model in config |

### Provider management

| # | Scenario | Expected |
|---|----------|----------|
| 1 | open `/providers` | provider table shown |
| 2 | select provider → "View Models" | model picker shown inside popup |
| 3 | select model from picker | default model updated, config written |
| 4 | cancel picker | return to action menu |
| 5 | refresh in picker | loading → updated list |

### CLI model listing

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `anie models` with configured providers | table printed to stdout |
| 2 | `anie models --provider ollama` | filtered to Ollama models |
| 3 | `anie models --refresh` | fresh data, not cached |
| 4 | `anie models` with no providers | helpful setup message |

### Persistence

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `/model` selection | session + runtime only (no config write) |
| 2 | onboarding selection | global config written |
| 3 | provider management selection | config written, path shown in message |
| 4 | resumed session restores model | session model takes effect |

### Performance

| # | Scenario | Expected |
|---|----------|----------|
| 1 | `/model` open with warm cache | < 50ms to show picker |
| 2 | `/model` refresh against local endpoint | < 2s |
| 3 | `/model` refresh against remote endpoint | < 5s |
| 4 | `anie models` cold start | < 5s for complete listing |
| 5 | picker with 50+ models | smooth scrolling, responsive search |

---

## Constraints

1. No new feature work in this phase. Only polish, docs, and validation.
2. Prefer clarifying user-facing messages over adding code.
3. If a validation failure requires a code fix, the fix should be minimal and targeted.

---

## Exit criteria

- [ ] all picker launch points render correctly (input-pane, card, popup)
- [ ] search, selection, cancel, refresh feel responsive and polished
- [ ] README updated with `/model` picker, `anie models`, `Ctrl+O`
- [ ] architecture docs updated for model discovery and onboarding flow
- [ ] design doc status updated
- [ ] manual QA matrix completed with no blocking issues
- [ ] all automated tests pass (`cargo test --workspace`)
- [ ] no regressions in existing onboarding, `/providers`, or `/onboard` flows

---

## Ship gate

Do not ship until:

- [ ] model discovery works for OpenAI-compatible, Anthropic, and Ollama
- [ ] `/model` uses a pi-style non-full-screen selector with search
- [ ] onboarding uses inline model selection inside its existing card
- [ ] provider management can browse and apply models
- [ ] `anie models` CLI command works
- [ ] persistence rules are documented and tested
- [ ] all tests green
