# PR 1 — Forced re-verification on tool failure

## Rationale

The 2026-05-01 smoke caught the model claiming
"The program compiled and ran successfully!" at T7
when bash had returned `[tool error]` (segfault). The
model parsed the error, then talked past it. With small
open models this happens often enough that a
system-prompt reminder is too weak — small models
ignore the existing "use web_search" line in the same
prompt under similar pressure.

The harness already has the failure information: every
tool result carries `is_error: bool`
(`crates/anie-protocol/src/messages.rs:75`). The
harness should *use* that signal structurally, not just
forward it.

## Design

When a `ToolResultMessage` comes back with
`is_error == true`, the harness wraps the content
before handing it to the model. The wrapping prepends
a directive block that the model has to consume to
read the actual error:

```
[harness-injected note]
The previous tool call FAILED. Before claiming
success or moving on, you MUST:
- For an `edit` or `write` failure: re-read the file
  with the `read` tool to see the current state.
- For a `bash` non-zero exit or runtime crash: re-run
  the failing command (or a diagnostic variant) and
  examine the output.
- For a `web_*` failure: try a different URL or
  search query.

Original tool error follows:
---
<original content>
```

Importantly, this rides on the *tool result* path
(content the model has to read to continue) — not on
the system-reminder side channel that the model can
skim past.

For bash specifically, when we see a non-zero exit
*without* the explicit `is_error` flag (some tools
return success-shaped output for failed commands), we
inspect the structured `details` for an `exit_code`
field and treat non-zero the same way.

## Files to touch

- `crates/anie-cli/src/controller.rs` — find the tool-
  result conversion path (where `ToolResultMessage`
  becomes a `Message::ToolResult` headed for the next
  model turn). Add a small `wrap_failed_tool_result`
  pass before the message is appended.
- `crates/anie-protocol/src/messages.rs` — no schema
  change; we read existing fields.
- `crates/anie-cli/src/controller.rs` (tests, or a new
  `tests` module) — unit tests for the wrapper.

Estimated diff: ~120 LOC of code, ~80 LOC of tests.

## Phased PRs

This is a single PR.

## Test plan

- `wrap_failed_tool_result_prepends_directive_when_is_error_true`
  — happy path.
- `wrap_failed_tool_result_passes_through_when_is_error_false`
  — no-op on success.
- `wrap_failed_tool_result_recognizes_bash_nonzero_exit_in_details`
  — failure mode where `is_error=false` but
  `details.exit_code != 0`.
- `wrap_failed_tool_result_keeps_original_content_intact`
  — verify the original error is appended verbatim
  (no truncation, no escaping that breaks small-model
  parsing).
- `wrap_failed_tool_result_directive_text_mentions_tool_name`
  — for edit/write failures, the directive points at
  re-read; for bash failures, at re-run; for web_* at
  retry-with-different-args.

Smoke check: re-run the 11-turn protocol; confirm T7
no longer hallucinates "compiled successfully" when
the binary segfaults.

## Risks

- **Frontier-model annoyance.** Sonnet/GPT-5/etc.
  already verify after failures; the directive is
  redundant noise. Mitigation: gate behind
  `--harness-mode=rlm` initially. Default off in
  `--harness-mode=current` and `--harness-mode=baseline`.
- **Prompt-injection vector.** A tool result whose
  body contains adversarial markdown could escape the
  directive block. Mitigation: the directive is a
  fixed prefix; downstream parsing doesn't depend on
  separator markers.
- **Increased token cost.** ~80 tokens per failed
  tool call. Acceptable.

## Exit criteria

- [ ] `wrap_failed_tool_result` implemented and wired
      into the tool-result path in
      `--harness-mode=rlm`.
- [ ] All five tests above pass.
- [ ] `cargo test --workspace` + `cargo clippy
      --workspace --all-targets -- -D warnings`
      clean.
- [ ] Smoke run T7 produces a non-hallucinated
      response (model acknowledges the failure rather
      than claiming success).
- [ ] `ANIE_DISABLE_FAIL_REVERIFY=1` env flag turns
      the wrapping off (for bisection).

## Deferred

- Auto-injecting a synthetic re-read tool call
  (option #2 from the discussion). PR 1 is the
  augmented-result form; auto-tool-call is a heavier
  intervention to add later if smoke shows the
  augmented form isn't enough.
- Per-tool-name directive customization beyond the
  three buckets above (edit/write, bash, web_*).
