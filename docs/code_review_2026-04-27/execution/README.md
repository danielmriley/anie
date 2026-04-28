# Execution tracker — code_review_2026-04-27

Status key:

- ⬜ Not started
- 🟡 In progress
- ✅ Landed
- ⏸ Deferred

| Plan | Status | Notes |
|---|---:|---|
| 01 — Repo formatting and CI hygiene | ✅ | Landed `5cf01ff` (2026-04-27). 828 lines of mechanical diff across 18 files; gates green. |
| 02 — `[ui]` config loading | ✅ | Landed `63f517b` (2026-04-27). PR A only; PR B (template) and PR C (docs) deferred until needed. Three loader tests pin behavior. |
| 03 — Web SSRF and redirect boundary | ✅ | PR A `ea5dd9f` (manual redirect validation). PR B `40ef55a` (DNS resolver abstraction + resolved-IP guard, IPv4-mapped IPv6, Class E reserved). PR C `8f34d62` (conservative headless gate: pre-launch DNS validation, runtime warning, doc + tool-description SSRF caveat). Chrome request interception deferred. |
| 04 — Web cancellation, budgets, and bounded side channels | 🟡 | PR A `8b09c8f` (cancellation threaded through fetch / robots / Defuddle / headless). PR B landed (bounded error body + robots + Defuddle stdout/stderr; oversized robots treated as unavailable; oversized Defuddle stdout surfaces typed error). PRs C/D (config + progress) still open. |
| 05 — Streaming built-in read path | ⬜ | Memory/resource hardening for large text files. |
| 06 — Ollama effective `num_ctx` error messaging | ✅ | Landed `13f2dda` (2026-04-27). One-line call-site fix in the give-up handler + targeted regression test. |
| 07 — robots.txt and Defuddle extraction correctness | ⬜ | Standards/extraction polish. |
| 08 — Atomic-write durability clarification | ⬜ | Low-risk durability/doc cleanup. |

## Global validation checklist

Run after each behavioral PR unless the PR is explicitly docs-only:

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`

Feature-specific checks as applicable:

- [ ] `cargo test -p anie-tools-web`
- [ ] `cargo check -p anie-tools-web --features headless`
- [ ] `cargo check -p anie-cli --features web-headless`
- [ ] Manual web smoke: `web_search` then `web_read` a public result.
- [ ] Manual abort smoke: abort an in-flight `web_read` and confirm prompt
      returns without leaked child processes.
- [ ] Manual config smoke: set `[ui]` values and confirm TUI startup picks
      them up.

## Notes

- Keep Plan 01 format-only. Do not hide behavior changes in the rustfmt
  diff.
- For Plan 04, do not land new hardcoded short timeouts without the
  config/budget story. Cancellation and memory caps can land first.
