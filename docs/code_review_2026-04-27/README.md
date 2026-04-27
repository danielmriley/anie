# code_review_2026-04-27: remediation plan set

This folder turns `docs/code_review_2026-04-27.md` into a set of
implementation-ready plans. The review found that the weekend work is
in good shape overall — tests and clippy pass, the Ollama native path is
well-covered, and the TUI performance work is structurally sound. The
remaining work is focused hardening around web-tool network boundaries,
config loading, resource usage, and a few correctness polish items.

## Timeout / long-running-agent policy

The review called out missing cancellation and subprocess timeouts in
`web_read`, but anie is intended to become a long-running persistent
agent. These plans therefore avoid sprinkling arbitrary short hard stops
through the code. The policy for this plan set is:

1. **Cancellation is mandatory; timeouts are policy.** A user abort,
   shutdown, or controller cancellation token must stop a tool promptly.
   That is a correctness requirement. Wall-clock timeouts should be
   explicit configuration, not hidden constants scattered through tool
   code.
2. **Prefer progress-aware budgets over total-runtime caps.** It is fine
   for a persistent agent to wait on a slow but still-progressing task.
   It is not fine to wait forever on a dead TCP read, stalled
   subprocess, or blocked child process with no output and no exit.
3. **Centralize web-tool knobs under config.** Existing constants can
   remain as defaults, but new behavior should be driven by a small
   `[tools.web]` config surface so operators can relax limits for
   long-running deployments.
4. **Hard caps protect memory, not impatience.** Byte caps on response
   bodies, error excerpts, robots.txt, and subprocess output are safety
   boundaries. They should remain even for persistent agents. Time caps
   should be generous, cancellable, and configurable.
5. **No silent background hangs.** If a tool is still running because a
   configured budget is long or disabled, the controller/UI should still
   be able to abort it and should not block unrelated async work.

## Ordering and dependencies

| # | Plan | Findings addressed | Priority | Depends on |
|---|---|---|---|---|
| 01 | [Repo formatting and CI hygiene](01_repo_formatting_ci.md) | #3 | High | none |
| 02 | [`[ui]` config loading](02_ui_config_loading.md) | #2 | High | none |
| 03 | [Web SSRF and redirect boundary](03_web_ssrf_redirect_boundary.md) | #1 | High | none |
| 04 | [Web cancellation, budgets, and bounded side channels](04_web_cancellation_budgets.md) | #4, #5 | High | 03 can land before or after |
| 05 | [Streaming built-in read path](05_streaming_read_cap.md) | #6 | Medium | none |
| 06 | [Ollama effective `num_ctx` error messaging](06_ollama_effective_num_ctx_message.md) | #7 | Medium | none |
| 07 | [robots.txt and Defuddle extraction correctness](07_robots_and_defuddle_correctness.md) | #8, #9 | Medium | 03 helpful |
| 08 | [Atomic-write durability clarification](08_atomic_write_durability.md) | #10 | Low | none |

## Suggested landing order

1. **Plan 01** first. It is a formatting-only unblocker for CI and
   should not be mixed with behavioral changes.
2. **Plan 02** next. It is small, user-visible, and unlocks the config
   surface needed by Plan 04.
3. **Plan 03** before expanding or promoting web-tool usage. The SSRF
   boundary is the only security-critical finding.
4. **Plan 04** after or alongside Plan 03. Cancellation can land before
   the richer config knobs, but config-driven budgets should follow
   quickly so we do not hardcode too many stops.
5. **Plans 05 and 06** are independent correctness/resource fixes.
6. **Plan 07** improves standards compliance and extraction quality once
   the main web safety boundary is in place.
7. **Plan 08** is a small durability/doc cleanup; land when convenient.

## Milestone exit criteria

- [ ] `cargo fmt --all -- --check` passes.
- [ ] `[ui]` settings loaded through `load_config_with_paths()` affect
      interactive startup.
- [ ] `web_read` rejects private destinations before any request is sent,
      including redirects.
- [ ] DNS rebinding/private resolved IPs are tested and blocked on the
      non-headless fetch path.
- [ ] `web_read` and `web_search` honor cancellation tokens.
- [ ] Web side-channel bodies (`robots.txt`, HTTP error bodies,
      Defuddle stdout/stderr) are bounded by memory caps.
- [ ] Tool time/budget knobs are centralized and configurable, with
      persistent-agent-friendly defaults and docs.
- [ ] Built-in `read` no longer loads an entire large text file just to
      return a truncated excerpt.
- [ ] Ollama load-failure messages report the effective `num_ctx` that
      was actually attempted.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Explicitly deferred

- A full browser sandbox for the headless path. Plan 03 requires a
  clear policy and safe default; full request interception / browser
  network sandboxing can be staged after the non-headless SSRF boundary
  is fixed.
- A general tool-permission or approval system. These findings are
  resource/boundary bugs inside the current full-access tool model, not
  a mandate to change that product boundary.
- Hard global wall-clock limits on all tools. Persistent-agent use cases
  need long-running operations; cancellation and configurable progress
  budgets are the right next step.

## References

- `docs/code_review_2026-04-27.md` — source review.
- `docs/code_review_2026-04-24/README.md` — previous review-plan
  structure.
- `docs/arch/anie-rs_architecture.md` — current architecture source of
  truth.
