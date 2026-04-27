# Execution tracker — code_review_2026-04-27

Status key:

- ⬜ Not started
- 🟡 In progress
- ✅ Landed
- ⏸ Deferred

| Plan | Status | Notes |
|---|---:|---|
| 01 — Repo formatting and CI hygiene | ⬜ | Format-only PR should land first. |
| 02 — `[ui]` config loading | ⬜ | Needed for documented UI prefs and future web-budget config. |
| 03 — Web SSRF and redirect boundary | ⬜ | Security-critical web-tool hardening. |
| 04 — Web cancellation, budgets, and bounded side channels | ⬜ | Keep timeout policy configurable/persistent-agent friendly. |
| 05 — Streaming built-in read path | ⬜ | Memory/resource hardening for large text files. |
| 06 — Ollama effective `num_ctx` error messaging | ⬜ | Small correctness fix. |
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
