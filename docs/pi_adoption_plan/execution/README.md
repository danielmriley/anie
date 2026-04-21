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
| 07 | OAuth | A (Credential tagged enum) | pending | — |
| 07 | OAuth | B (OAuthProvider trait + Anthropic impl) | pending | — |
| 07 | OAuth | C (refresh-with-lock) | pending | — |
| 07 | OAuth | D (CLI + TUI integration) | pending | — |
| 07 | OAuth | E (second provider, optional) | pending | — |

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
