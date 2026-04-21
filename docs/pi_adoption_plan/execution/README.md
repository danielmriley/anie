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
| 05 | Markdown renderer | A (scaffolding + parser) | pending | — |
| 05 | Markdown renderer | B (code blocks + syntax) | pending | — |
| 05 | Markdown renderer | C (lists/tables/quotes) | pending | — |
| 05 | Markdown renderer | D (links + OSC 8) | pending | — |
| 05 | Markdown renderer | E (ship — flip default) | pending | — |

## Tier 4 — significant depth

| # | Plan | PR | Status | Commit |
|---|------|----|--------|--------|
| 06 | Compaction fidelity | A (schema v4 + details field) | pending | — |
| 06 | Compaction fidelity | B (file-op extraction) | pending | — |
| 06 | Compaction fidelity | C (split-turn summaries) | pending | — |
| 06 | Compaction fidelity | D (resume exposure, polish) | pending | — |
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
