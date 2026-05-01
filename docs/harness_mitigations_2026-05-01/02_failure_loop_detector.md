# PR 2 — Failure-loop detector (observability only)

## Rationale

The 2026-05-01 smoke saw T7 wedge for 14 minutes:
qwen3.5:9b kept issuing the same broken bash invocation
after `[tool error]`, with no adaptation between
attempts. PR 1 attacks the ignore-the-failure pattern
at the message level. PR 2 makes the loop *visible* —
to logs, to the TUI, and to anyone reviewing the
session — without aborting it.

Per the series principle: no hard caps. The detector
warns; the user (or a separate process) decides
whether to interrupt.

## Design

Track consecutive tool failures per
`(tool_name, args_hash)` pair on the controller. A new
"strike" increments when:

- Same tool name is called.
- The arguments hash (stable hash of normalized JSON)
  matches the previous strike.
- The previous result was `is_error == true`.

A successful call (or a call with different
args_hash) resets the counter for that pair.

When the counter reaches a configurable threshold
(default `3`, adjustable via
`ANIE_FAILURE_LOOP_WARN_AT`), the harness:

- Emits an `info!`-level log line:
  `failure_loop_detected: tool=<name>
  args_hash=<hash> strikes=<n> first_seen_at=<ts>`.
- Surfaces a status-bar line in the TUI:
  `loop: <tool> failed N times` so the user can see it
  without scrolling.
- Appends a single ledger entry on the next ledger
  injection: `loop_warning: <tool> repeated N times
  with same args`.

That's the entire intervention: warn loudly, abort
nothing. If the smoke shows users are missing the
warning and these loops still wedge the session, we
add an opt-in abort in a follow-up PR.

## Files to touch

- `crates/anie-cli/src/controller.rs` — add
  `FailureLoopDetector` struct; track and emit
  warnings.
- `crates/anie-cli/src/context_virt.rs` — extend the
  ledger output to include any active loop warnings.
- `crates/anie-cli/src/tui/status.rs` (or wherever the
  status bar is composed) — render the warning line
  when set.
- Tests in the same crate.

Estimated diff: ~150 LOC of code, ~100 LOC of tests.

## Phased PRs

Single PR.

## Test plan

- `loop_detector_strikes_only_when_args_hash_matches`
  — different args don't accumulate.
- `loop_detector_resets_on_success`
  — a clean call clears strikes.
- `loop_detector_emits_warning_at_configured_threshold`
  — warn at strike 3 (default), 5 (env override), etc.
- `loop_detector_does_not_abort_session`
  — even at strike 100, the loop continues.
- `loop_detector_args_hash_stable_across_field_order`
  — JSON object key reordering doesn't change the
  hash.

## Risks

- **Hash false-positives.** Two functionally different
  calls could collide on a hash. Acceptable — false
  positive is a spurious warning, not a failure.
- **Hash false-negatives.** Same call with a trivial
  whitespace difference (e.g., model added a trailing
  newline to a bash command between attempts) would
  not match. Mitigation: normalize via
  `serde_json::Value::canonical_form()` (alphabetize
  keys, normalize whitespace) before hashing.
- **Warning fatigue.** If many short loops fire the
  warning, users start ignoring the status bar.
  Mitigation: throttle — warn once per
  `(tool_name, args_hash)` per session, even if the
  loop continues.

## Exit criteria

- [ ] `FailureLoopDetector` lives on the controller
      and is consulted on every tool result.
- [ ] All five tests above pass.
- [ ] `cargo test --workspace` + `cargo clippy
      --workspace --all-targets -- -D warnings` clean.
- [ ] Smoke run: when a deliberate retry loop is
      forced (e.g., bash that always errors), the
      warning appears in the TUI status bar by attempt
      3 and in the log.
- [ ] `ANIE_DISABLE_LOOP_DETECTOR=1` env flag turns
      detection off entirely.

## Deferred

- Auto-abort at threshold. Add only if observability
  proves insufficient.
- Per-tool threshold customization.
- Detecting *near*-duplicate args (e.g., model varied
  one character between attempts).
