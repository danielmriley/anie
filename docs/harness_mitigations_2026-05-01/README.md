# Harness mitigations for small-model failure modes (2026-05-01)

A three-PR plan series targeting the loudest failure
modes from the 2026-05-01 11-turn smoke (session
`ab03cc6f`, qwen3.5:9b). See
`docs/smoke_protocol_2026-05-01.md` for the scenario;
the failure modes those PRs address are tracked there.

## Principles

- **Observability over hard caps.** No pre-emptive
  numeric ceilings on retries, depth, fan-out, or token
  budget. Surface what's happening (logs, TUI status,
  ledger lines); rely on user-side interruption when a
  loop is unproductive. Add caps later only if the
  smoke shows a class of failure that observability
  alone can't make safe.
- **Structural injection beats system-prompt nudges.**
  When the harness can see a failure directly (tool
  return values, edit-without-rerun patterns), prefer
  intercepting the data stream the model has to
  consume over adding lines to the system prompt that
  small models can ignore.
- **Default-on, opt-out via env.** Each mitigation
  ships enabled by default in `--harness-mode=rlm` and
  can be turned off with an `ANIE_*` env flag for
  smoke-test bisection.

## PRs in order

| PR | Doc | Failure mode it addresses | Status |
|---|---|---|---|
| 1 | [01_forced_reverification.md](01_forced_reverification.md) | T7 hallucinated "compiled and ran successfully!" after `[tool error]` | **shipped** (`e7abe6a`) |
| 2 | [02_failure_loop_detector.md](02_failure_loop_detector.md) | T7 sat 14 min issuing the same broken bash call without adapting | **shipped** (`a2b4373`) |
| 3 | [03_system_prompt_retest.md](03_system_prompt_retest.md) | T5 introduced infinite recursion; never re-ran the binary | **shipped + revised** (`5d3ff1a` + `495a6bb` follow-up moving rule out of base prompt) |
| 4 | [04_failed_tool_result_eviction.md](04_failed_tool_result_eviction.md) | "Context rot" from failed tool results that linger after they're no longer relevant (Cursor harness post-mortem 2026-05-02) | **shipped (Signal A — supersession)**: failed tool results whose `(tool_name, args_hash)` match a later successful call evict ahead of FIFO order. Signal B (low-relevance via embedding) deferred — Signal A may be sufficient on smoke evidence. |

PRs 1-3 land first because they target the loudest
failure modes. PR 4 was added after the post-PR-3
smoke + the Cursor article confirmed that wrapping
failures alone (PR 1) doesn't solve the second-order
problem of failures degrading subsequent decisions.
Together they cover both halves: "the model engages
with the failure" (PR 1) and "the failure doesn't
poison context indefinitely" (PR 4).

## Exit criteria for the series

- [ ] All three PRs land on `dev_rlm`.
- [ ] `cargo test --workspace` and
      `cargo clippy --workspace --all-targets -- -D
      warnings` are clean after each.
- [ ] Re-run the 11-turn smoke protocol against
      qwen3.5:9b. Compare the result table:
  - T7 hallucinated success → expected: **fixed** (PR 1).
  - T7 wedged > 10 min → expected: **fixed** in
    practice because PR 1 keeps the model from
    hallucinating its way past the failure; PR 2's
    detector also flags the loop in logs.
  - T5 infinite recursion still introduced →
    acceptable; PR 3 surfaces the rule but doesn't
    enforce. Surfacing in the binary at T7's recompile
    + run step is what we'd hope to see.
- [ ] No regression on T1-T6, T8-T11.

## What's deferred to follow-up plans

- True sub-agents (recurse with full tool access).
  Tracked under `docs/rlm_subagents_2026-05-01/`.
- Decompose-then-execute scaffolding.
- Parallel recurse + voting.

These are the long-tail-reasoning levers; mitigations
land first because they fix loud bugs the next smoke
will already catch.
