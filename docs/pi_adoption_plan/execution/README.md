# Pi-adoption execution tracker

Status of each plan's PRs. Update inline as work lands.

## Tier 1 — tiny, ship first

| # | Plan | PR | Status | Commit |
|---|------|----|--------|--------|
| 01 | Compat knobs — maxTokensField + "minimal" | A (maxTokensField) | landed | `787c13a` |
| 01 | Compat knobs — maxTokensField + "minimal" | B (minimal) | landed | `1945add` |
| 03 | Token estimation — usage-seeded | (single PR) | landed | `2855ebc` |

## Tier 2 — low risk, clearly scoped

| # | Plan | PR | Status | Commit |
|---|------|----|--------|--------|
| 02 | Search tools | A (grep) | landed | `3c5bc0c` |
| 02 | Search tools | B (find) | landed | `8aacfd8` |
| 02 | Search tools | C (ls) | landed | `8aacfd8` |
| 02 | Search tools | D (slash commands, optional) | deferred | — |
| 04 | Terminal capabilities | (single PR) | landed | `68ee56e` |

## Tier 3 — structural but scoped

| # | Plan | PR | Status | Commit |
|---|------|----|--------|--------|
| 05 | Markdown renderer | A (scaffolding + parser) | landed | `6450bc1` |
| 05 | Markdown renderer | B (code blocks + syntax) | landed | `0f48a52` |
| 05 | Markdown renderer | C.1 (lists + blockquotes) | landed | `48a4fb4` |
| 05 | Markdown renderer | C.2 (tables) | landed | `a4baec3` |
| 05 | Markdown renderer | D (links + inline images, OSC 8 deferred) | landed | `f22b741` |
| 05 | Markdown renderer | E.1 (wire into output pane) | landed | `3720627` |
| 05 | Markdown renderer | E.2 (`/markdown on\|off`) | landed | `0eb6a1b` |

**Plan 05 notes:**
- C split into C.1 + C.2 so table rendering (collect-then-emit)
  landed as a separate reviewable unit from list / blockquote
  prefix handling.
- OSC 8 hyperlink emission (PR D) is deferred — rationale in
  `crates/anie-tui/src/markdown/link.rs`: ratatui's
  unicode-width accounting counts the URL body inside the OSC 8
  escape as visible cells, breaking layout. The fallback path
  (visible ` (url)` after link text) is the default. Revisit
  when ratatui ships native hyperlink support or we find a
  backend-level workaround.
- Not yet verified: "No observable TUI latency regression on a
  long agent run." and "Manual visual smoke against 3+ real
  assistant responses." Both require running the TUI against a
  live provider; pending a dedicated smoke session.

## Tier 4 — significant depth

| # | Plan | PR | Status | Commit |
|---|------|----|--------|--------|
| 06 | Compaction fidelity | A (schema v4 + details field) | landed | `d8bf578` |
| 06 | Compaction fidelity | B (file-op extraction) | landed | `bced0ce` |
| 06 | Compaction fidelity | C.1 (find_cut_point struct refactor) | landed | `0708b32` |
| 06 | Compaction fidelity | C.2 (split-turn two-summary join) | landed | `90c9c77` |
| 06 | Compaction fidelity | D (resume exposure) | landed | `6876d42` |

**Plan 06 notes:**
- C split into C.1 + C.2 per CLAUDE.md §6 (refactor first, feature
  second). C.1 is the type-signature change; C.2 populates
  split_turn + adds the two-summary join.
- PR D's "system-prompt integration" (injecting a
  recently-touched-files hint at resume) is deferred per the
  plan's explicit "optional polish" framing. The accessors
  land so wiring that later is a one-site edit.
- Not yet verified: the two manual exit criteria ("30+ message
  session compacts mid-turn" and "`jq` inspection of a real
  session") — both need a live provider session.

| # | Plan | PR | Status | Commit |
|---|------|----|--------|--------|
| 07 | OAuth | A (Credential tagged enum) | landed | `91f6f82` |
| 07 | OAuth | B (OAuthProvider trait + Anthropic impl) | landed | `c91af1b` |
| 07 | OAuth | C (refresh-with-lock) | landed | `eb27cc2` |
| 07 | OAuth | D.1 (anie login/logout CLI + AuthResolver) | landed | `8c3555a` |
| 07 | OAuth | D.1+ (auto-open browser, compact prompt) | landed | `a35f37a` |
| 07 | OAuth | E.1 (trait refactor: device flow + credential extras) | landed | `d71e533` |
| 07 | OAuth | E.2 (OpenAI Codex provider) | landed | `eda8448` |
| 07 | OAuth | E.3 (GitHub Copilot provider, device flow) | landed | `4a79550` |
| 07 | OAuth | E.4 (Google Antigravity provider) | landed | `0c22b64` |
| 07 | OAuth | E.5 (Google Gemini CLI provider) | landed | `2891169` |
| 07 | OAuth | F (OAuth provider routing + model catalog) | landed | `47e6f37` |
| 07 | OAuth | G (fix --provider resolver + Copilot headers) | landed | `3d16f8a` |
| 07 | OAuth | D.2 (TUI OAuth surface: picker + /providers + /login) | landed | `3a733c4` |
| 07 | OAuth | D.2+ (headers on all discovery paths) | landed | `480160f` |
| 07 | OAuth | D.2++ (Copilot chat-model filter + saved-config fix) | landed | `f9bba9e` |

**Plan 07 notes:**
- PR D split into D.1 (CLI) + D.2 (TUI polish). D.1 stands on
  its own: `anie login <provider>` runs the full OAuth flow
  against production endpoints, and agent runs thereafter pick
  up the OAuth credential automatically via AuthResolver +
  refresh-with-lock.
- D.1+ followup: `anie login` now auto-opens the browser
  (opener crate) and collapses the terminal output to three
  compact lines.
- Endpoints + client IDs verified against pi's corresponding
  TS files on 2026-04-21 — each provider's module doc records
  the line numbers.
- PR E broken into E.1–E.5. E.1 extended the trait (LoginFlow
  became an enum, DeviceCodeFlow added, `api_base_url` /
  `project_id` added to credential shape). E.2–E.5 each added
  one provider with 9–13 wiremock-backed tests.
- PRs F + G + D.2 each shipped with multiple follow-ups after
  live smoke caught silent bugs: resolver falling through to
  the wrong provider, missing headers on parallel TUI
  discovery paths, and unfiltered Copilot model catalog
  (non-chat models crashing chat requests). All caught only by
  running against the real endpoint.
- Five providers registered: `anthropic`, `openai-codex`,
  `github-copilot`, `google-antigravity`, `google-gemini-cli`.
- End-to-end smoke verified with GitHub Copilot on 2026-04-21
  (login → model discovery → chat). Anthropic / Codex / Gemini
  unshipped-to-user verification — providers technically
  complete but Anthropic actively enforces third-party-agent
  ToS so don't smoke it.

**Plan 07 deferred:**
- Onboarding first-run flow offering OAuth as a preset. `anie
  login <provider>` works standalone; surfacing it inside
  onboarding is a UX polish task.
- Live refresh-with-lock exercise. Copilot access tokens
  expire ~30 min after login; the next `anie` run after expiry
  should transparently refresh. Not yet seen in the wild.

## Suggested landing order

Tier 1 first (one week, cumulative):
1. 01A — maxTokensField compat
2. 01B — minimal thinking level
3. 03 — usage-seeded token estimation

Tier 2 in parallel with Tier 3 start:
4. 02A — grep tool
5. 04 — terminal capabilities
6. 02B — find tool
7. 02C — ls tool

Tier 3 sequentially (markdown is the biggest user-facing win):
8. 05A → 05E over several PRs

Tier 4 when Tier 3 is stable:
9. 06A → 06D
10. 07A → 07D

## Per-PR gate

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus a plan-specific manual smoke documented in that plan's exit
criteria.

## Where this plan came from

- `docs/anie_vs_pi_comparison.md` — functional survey.
- `docs/pi_adoption_plan/README.md` — plan index.
- Prior template: `docs/max_tokens_handling/README.md`.
