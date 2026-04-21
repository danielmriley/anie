# Pi-adoption plan

Detailed implementation plans for every priority item surfaced in
`docs/anie_vs_pi_comparison.md`. Seven focused plans, ordered by
cost-adjusted signal so the cheap fast wins land first and the
structurally-invasive ones land after we've confirmed them with the
smaller changes.

## Guiding principles

1. **Small, verifiable PRs.** Every plan breaks into numbered
   steps of roughly one commit each. Each PR's exit criteria are
   concrete (tests added, clippy clean, a specific behavior
   change verifiable on the wire or in a snapshot).
2. **Keep the protocol crate pure.** `anie-protocol` stays
   dependency-minimal. Anything new that touches persistence gets
   a schema-version bump + forward-compat tests.
3. **Reuse before invent.** If ratatui or `termimad` or a Rust
   crate does the job, wire it up instead of writing our own.
   We'll copy pi's *structure*, not its literal code.
4. **Respect the comparison's "not worth copying" list.** Don't
   back-port regex-based error classification, 15 Api variants,
   or pi's line-level diff renderer. The comparison doc flagged
   these explicitly; re-check before deviating.

## Ordering and dependencies

```
Tier 1 — tiny changes, low risk, big cumulative signal
  01 compat knobs (maxTokensField, minimal thinking)
  03 token estimation (use provider-reported usage)
Tier 2 — low risk, clearly scoped, independent
  02 search tools (grep/find/ls)
  04 terminal capabilities (probing + gating)
Tier 3 — structural, but well-scoped
  05 markdown renderer in OutputPane
Tier 4 — significant depth
  06 compaction fidelity (split-turn + file-ops)
  07 OAuth auth
```

Dependencies:

- **05** (markdown) benefits from **04** (capability detection)
  for OSC-8 hyperlink gating, but can ship without it and add a
  "never emit hyperlinks" fallback.
- **06** (compaction) is independent of everything else but
  depends on compaction stability — land it after any pending
  compaction-adjacent work settles.
- **07** (OAuth) is independent structurally; it does touch
  `anie-auth` which most other plans don't, so it can happen in
  parallel with the TUI-facing items.

## Plans

| # | Plan | Tier | Size | File |
|---|------|------|------|------|
| 01 | Compat knobs — `maxTokensField`, `"minimal"` thinking level | 1 | Small | [01_compat_knobs.md](01_compat_knobs.md) |
| 02 | Built-in search tools — `grep`, `find`, `ls` | 2 | Medium | [02_search_tools.md](02_search_tools.md) |
| 03 | Provider-reported token usage for compaction triggering | 1 | Small | [03_token_estimation.md](03_token_estimation.md) |
| 04 | Terminal capability detection | 2 | Small-Medium | [04_terminal_capabilities.md](04_terminal_capabilities.md) |
| 05 | Markdown rendering in the TUI | 3 | Large | [05_markdown_renderer.md](05_markdown_renderer.md) |
| 06 | Compaction fidelity — split-turn summaries + file-op tracking | 4 | Large | [06_compaction_fidelity.md](06_compaction_fidelity.md) |
| 07 | OAuth auth (Claude Code-style) | 4 | Largest | [07_oauth_auth.md](07_oauth_auth.md) |

## Milestone exit criteria (when we've "caught up" to pi)

- [ ] All seven plans' exit criteria met.
- [ ] `cargo test --workspace` green; `cargo clippy --workspace
      --all-targets -- -D warnings` clean.
- [ ] Manual smoke across four scenarios:
  - OpenRouter multi-turn with reasoning model (tests 01, 03, 05).
  - Long session that triggers compaction mid-turn (tests 06).
  - First-run onboarding with OAuth (tests 07).
  - Agent runs that use `grep` tool against a small repo (tests 02).

## What's not in this plan

The comparison doc's "not worth copying" list:

- pi's 15 Api variants. Skip.
- pi's regex error classification. We already have better.
- pi's component-local cache. Our block-local cache in
  `OutputPane` covers the same ground.

Also excluded: any features unique to pi that aren't currently
gapping anie's UX or correctness (e.g., pi's inline image
rendering, pi's sophisticated input editor with undo/kill-ring —
these are genuinely nice but pi-ahead-of-us isn't a pressing
signal; they land when we feel them missing).

## References

- `docs/anie_vs_pi_comparison.md` — the findings this plan is
  operationalizing.
- `docs/max_tokens_handling/README.md` — the template all of
  these follow (evidence → plan → staged PRs).
- `docs/add_providers/README.md` — another prior multi-plan
  folder worth looking at for structure.
