# PR 3 — System-prompt amendment for re-test after edit

## Rationale

T5 of the 2026-05-01 smoke introduced an infinite
recursion in `insertHead(T&& value) {
insertHead(std::forward<T>(value)); }`. The model
edited `dll.hpp` and never re-ran the binary built in
T3. The bug surfaced two turns later when T7 tried to
recompile and the program segfaulted.

This is a behavior the system prompt can address:
explicitly instruct the model that any edit/write
must be followed by re-running the most recent
verification command (build, test, etc.) before the
turn ends.

PR 3 is a system-prompt-only change. If the smoke
shows qwen3.5:9b ignores it (as it ignored the
existing "use web_search" line), we layer
harness-side enforcement on top in a follow-up PR.

## Design

Append to the existing system prompt
(`crates/anie-cli/src/controller.rs:2091` area, the
"You are an expert coding assistant..." block):

```
- After any edit or write to a file under test, you
  MUST re-run the most recent verification command
  (build, test suite, or the script you ran to verify
  it works) BEFORE claiming the change works.
  Do not assume the change compiles or behaves as
  intended without verification. If you can't recall
  the verification command, ask the user before
  declaring success.
```

Also append to `RLM_SYSTEM_PROMPT_AUGMENT` (the rlm-
mode block) a re-test-before-claim reminder that ties
into the recurse tool's awareness of prior tool calls:
the ledger lists the bash commands run; the model
should grep that ledger for the most recent
build/test command after editing.

## Files to touch

- `crates/anie-cli/src/controller.rs` — system prompt
  string + RLM augment string.
- Tests: add a snapshot test that pins the new lines
  so they don't drift silently.

Estimated diff: ~30 LOC of code, ~20 LOC of tests.

## Phased PRs

Single PR.

## Test plan

- `system_prompt_includes_retest_directive` — string
  contains the new line.
- `rlm_augment_includes_retest_directive` — RLM
  augment also contains a retest line.
- Smoke run: T5 followed by T6 — does qwen3.5:9b
  re-run the binary after the edit? Compare to
  baseline.

This is a low-confidence smoke comparison — small
models often ignore system-prompt rules under context
pressure. We measure the rate; we don't expect 100%.

## Risks

- **Prompt bloat.** Already long. Mitigation: keep
  the new line tight (~30 tokens).
- **Frontier-model verbosity.** Sonnet-class models
  may follow the rule too literally and run tests
  after trivial edits. Mitigation: scope it to
  "files under test" — the model decides what counts.

## Exit criteria

- [ ] System prompt contains the retest line.
- [ ] RLM augment contains the retest line.
- [ ] Both snapshot tests pass.
- [ ] Smoke comparison run logs show the model
      re-running tests after edit at a rate >= the
      pre-PR baseline. Not a hard gate (rate may
      still be low for small models); we just want
      it to not regress.

## Deferred

- Harness-side detection of "edit without rerun"
  patterns. Layer on top of PR 3 only if the smoke
  shows the system-prompt approach isn't moving the
  needle.
