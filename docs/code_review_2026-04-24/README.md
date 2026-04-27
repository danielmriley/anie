# code_review_2026-04-24: implementation plan set

This folder turns `code-review_gpt5.5_04-24-2026.md` into a
set of implementation-ready plans. The review found a healthy
architecture overall: small crates, typed provider errors,
append-only sessions, bounded tool output, and a clear
provider/tool split. The remaining work is not a rewrite; it is a
sequence of focused hardening passes around async auth, provider
compatibility, retry state, persistence visibility, resource caps,
and repository hygiene.

The review was cross-checked against two reference implementations:

- `pi` at `/home/daniel/Projects/agents/pi`
- Codex at `/home/daniel/Projects/agents/codex`

That comparison changed the priority order. pi supports anie's
current full-access filesystem tool model, while Codex demonstrates
the heavier sandbox/approval architecture anie should not copy
unless the product boundary changes toward untrusted code. The
future isolation direction for anie remains WASM/containerized tool
execution, not incremental cwd pseudo-sandboxing.

## Already resolved before this plan set

These review items have already been handled and should not be
re-planned here:

| Review finding | Status |
|---|---|
| Windows `ls.rs` Unix-only import | Fixed by moving `PermissionsExt` under `#[cfg(unix)]`; Windows target check passed. |
| Tool cwd/full-access ambiguity | Resolved as intentional full system access; docs and tool descriptions now say absolute paths and `..` are allowed. |
| Bash command denylist idea | Implemented as `[tools.bash.policy]` accidental-risk guardrail, explicitly not a sandbox. |
| Architecture source of truth | `docs/arch/anie-rs_architecture.md` now owns current design patterns and risk guidance. |

## Guiding principles

1. **Do not replace the architecture.** Each plan should preserve the
   crate boundaries documented in `docs/arch/anie-rs_architecture.md`.
   If a change would move responsibilities between crates, update the
   architecture doc in the same PR.
2. **Small, verifiable PRs.** Every plan below is broken into
   reviewable slices. Prefer one behavior change plus tests per commit.
3. **Reference implementations guide, not dictate.** Copy pi's shape
   where it matches anie's goals. Borrow Codex's hardening patterns
   selectively. Do not import Codex's full approval/sandbox system into
   anie while anie remains a single-user harness with full-access tools.
4. **Keep typed errors.** anie's `ProviderError` and auth error
   taxonomy are strengths. New failures should be explicit variants or
   surfaced results, not string-matched logs.
5. **Async paths must not block Tokio workers.** If a filesystem or
   OS lock can block for human-visible time, isolate it with
   `spawn_blocking` or a genuinely async wait loop.
6. **Resource caps must be runtime-enforced.** JSON schemas are useful
   model guidance, but tool/resource limits need Rust-side checks too.

## Ordering and dependencies

| # | Plan | Review findings | Size | Depends on |
|---|------|-----------------|------|------------|
| 01 | [OAuth refresh lock async isolation](01_oauth_refresh_async_lock.md) | #3 | Small-Medium | none |
| 02 | [OpenAI structured image serialization](02_openai_structured_images.md) | #4 | Medium | none |
| 03 | [Retry-state consistency for model/thinking changes](03_retry_state_consistency.md) | #5 | Small-Medium | none |
| 04 | [OAuth callback per-connection deadlines](04_oauth_callback_deadlines.md) | #6 | Small | none |
| 05 | [Anthropic truncation classification](05_anthropic_truncation_classification.md) | #7 | Small-Medium | none |
| 06 | [Runtime-state persistence visibility](06_runtime_state_persistence_visibility.md) | #8 | Medium | none |
| 07 | [Atomic write hardening](07_atomic_write_hardening.md) | #9 | Medium | after 06 is understood |
| 08 | [Tool edit resource caps](08_tool_resource_caps.md) | #10 | Small-Medium | none |
| 09 | [TUI agent-event drain bounds](09_tui_event_drain_bounds.md) | #11 | Medium | coordinate with TUI perf work |
| 10 | [Repo hygiene, CI, and print-mode error visibility](10_repo_hygiene_ci.md) | #12-#16 | Small-Medium | none |

## Suggested landing order

1. **Plan 01** — highest confidence and directly affects runtime
   responsiveness. Keep anie's cross-process `fs4` lock, but stop
   sleeping on Tokio workers.
2. **Plan 02** — concrete provider compatibility gap. pi already shows
   the right OpenAI wire shape.
3. **Plan 03** — fixes surprising state consistency during armed retry
   and has straightforward controller tests.
4. **Plans 04 and 05** — auth/provider correctness follow-ups with
   clear failure modes.
5. **Plans 06 and 07** — persistence reliability. Land persistence
   visibility before changing the shared atomic-write primitive so
   callers have somewhere to surface write failures.
6. **Plans 08 and 09** — resource and responsiveness guardrails.
7. **Plan 10** — hygiene, CI hardening, metadata, and print-mode polish.

## Milestone exit criteria

- [ ] Plans 01-10 landed or explicitly deferred with rationale.
- [ ] `docs/arch/anie-rs_architecture.md` updated for any
      architecture-significant change.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Manual smoke covers:
  - OAuth token refresh while another process holds the refresh lock.
  - OpenAI-compatible image-capable model receiving a real image.
  - Retry backoff with attempted model/thinking changes.
  - Tool-heavy session using `read`, `grep`, `find`, `bash`, and `edit`.
  - Long streaming TUI run with rapid terminal input.

## What's intentionally not in this plan set

- Replacing full-access tools with cwd confinement. That was rejected
  for now; future isolation should be WASM/containerization.
- Adopting Codex's full approval-flow and sandbox stack. It is useful
  reference material, but it is not the right next step for anie.
- Reworking the provider trait or session schema unless a specific
  plan requires it.
- Broad performance refactors already covered by
  `docs/code_review_performance_2026-04-21/` and
  `docs/tui_responsiveness/`.

## References

- `code-review_gpt5.5_04-24-2026.md` — source review.
- `docs/arch/anie-rs_architecture.md` — current architecture source of
  truth.
- `docs/max_tokens_handling/README.md` — single-topic plan template.
- `docs/pi_adoption_plan/README.md` — multi-plan folder template.
- `docs/code_review_performance_2026-04-21/README.md` — earlier review
  plan-set structure.

