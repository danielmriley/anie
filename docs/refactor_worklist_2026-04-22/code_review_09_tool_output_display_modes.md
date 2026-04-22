# code_review_performance_2026-04-21 / 09: tool output display modes

## Rationale

Today the interactive transcript always builds and renders the full
tool-result body for completed tools:

- live tool completions go through `tool_result_body(...)` before
  `OutputPane::finalize_tool_result(...)`
  (`crates/anie-tui/src/app.rs:483-499`, `1664-1700`)
- replayed transcript entries do the same through
  `tool_result_message_body(...)`
  (`crates/anie-tui/src/app.rs:1359-1368`, `1683-1700`)
- `OutputPane` then always renders that body inside the boxed tool
  block (`crates/anie-tui/src/output.rs:525-547`)

That is the right default for `edit` and `write`, because their
file-change output is often the useful part, but it is often too noisy
for successful `bash` and `read` calls. The tool title formatting is
already good for a title-only mode:

- `bash` titles render as `$ <command>`
- `read` titles render as `read <path>`
  (`crates/anie-tui/src/output.rs:880-888`)

There is also already a precedent for **UI-only transcript settings**
that live in `UiConfig`, flow through `interactive_mode`, and mutate the
`OutputPane` render context at runtime:

- `UiConfig.markdown_enabled`
  (`crates/anie-config/src/lib.rs:38-66`)
- startup wiring in `interactive_mode`
  (`crates/anie-cli/src/interactive_mode.rs:35-45`)
- runtime toggling in `App` / `OutputPane`
  (`crates/anie-tui/src/app.rs:297-323`, `792-817`,
  `crates/anie-tui/src/output.rs:67-156`)
- slash-command catalog support for `/markdown`
  (`crates/anie-cli/src/commands.rs:31-33`, `258-266`)

Finally, print mode already behaves much closer to the desired compact
shape — it prints tool start hints but not tool-result bodies
(`crates/anie-cli/src/print_mode.rs:70-77`). So this plan should target
the interactive transcript, not broaden into print-mode behavior unless
that becomes a separate request.

## Design

### 1. Add a UI-only `tool_output_mode` setting

Add a new `UiConfig` field with two values:

- `verbose` — current behavior
- `compact` — the better name for the requested "short" mode

This stays UI-only, like `markdown_enabled`: it must not affect the
agent loop, provider payloads, or session storage shape. The transcript
should still retain the full `ToolResultMessage`; only rendering changes.

### 2. Gate rendering, not transcript storage

Do **not** strip tool bodies in `tool_result_body(...)` or
`tool_result_message_body(...)`. Those helpers feed both live rendering
and replay, and a data-dropping change would make it impossible to
switch back to `verbose` later in the same session.

Instead:

1. keep the full tool result content in `RenderedBlock::ToolCall`
2. thread `tool_output_mode` through the existing `OutputPane`
   render-context path
3. choose the body at render time

That makes the feature symmetric with markdown toggling and keeps cache
invalidation local to the pane.

### 3. Compact mode only suppresses successful `bash` / `read` bodies

In `compact` mode:

- successful `bash` blocks show the existing `$ <command>` title only
- successful `read` blocks show the existing `read <path>` title only
- `edit` / `write` remain fully visible, especially diffs when present
- other tools keep their existing output for this first pass

This keeps the request narrow and avoids accidental loss of important
non-file, non-shell tool output.

### 4. Preserve error visibility

Failed `bash` / `read` calls should still show their error text. A pure
"title only" rendering on failures would make debugging much worse and
would be inconsistent with the rest of the transcript.

So the compact-mode gate should be:

- **suppress only successful** `bash` / `read` bodies
- preserve `is_error` rendering and body text on failures

### 5. Reuse the `/markdown` runtime-toggle pattern

Expose the setting via a new UI-only slash command:

```text
/tool-output [verbose|compact]
```

Behavior:

- no arg: report current state
- `verbose`: enable full bash/read bodies again
- `compact`: hide successful bash/read bodies

Like `/markdown`, this should be handled entirely in the TUI and should
not dispatch a controller action.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-config/src/lib.rs` | add `UiConfig.tool_output_mode` and its default/serde shape |
| `crates/anie-cli/src/interactive_mode.rs` | thread the configured mode into `App` at startup |
| `crates/anie-cli/src/commands.rs` | register `/tool-output` and enumerated values |
| `crates/anie-tui/src/app.rs` | add builder/setter plumbing and slash-command handling |
| `crates/anie-tui/src/output.rs` | add render-context toggle and compact-mode gating for successful `bash` / `read` |
| `crates/anie-tui/src/tests.rs` | replay/live transcript and slash-command tests |
| `crates/anie-cli/src/controller_tests.rs` or config tests | only if a startup/config plumbing regression test belongs there |

## Phased PRs

### PR A — config + pane plumbing

1. Add `UiConfig.tool_output_mode` with `verbose` default and
   `compact` as the alternate value.
2. Thread it through `interactive_mode` into `App` / `OutputPane`.
3. Add cache invalidation on mode changes, mirroring
   `set_markdown_enabled`.
4. Do **not** change rendering behavior in this PR yet.

### PR B — compact rendering for `bash` / `read`

1. Add a render-time branch in `output.rs` for successful `bash` /
   `read` tool blocks.
2. Keep the existing title formatting unchanged.
3. Preserve full rendering for:
   - `edit`
   - `write`
   - errors from `bash` / `read`
   - every other tool
4. Ensure transcript replay and live tool completions behave the same.

### PR C — `/tool-output` runtime toggle

1. Add the command catalog entry and enumerated arguments
   (`verbose`, `compact`).
2. Mirror `/markdown` behavior:
   - no arg reports current mode
   - changing the mode updates only the UI
   - no controller action is dispatched
3. Add focused tests for command validation and output-pane state.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `ui_config_tool_output_mode_defaults_to_verbose` | config tests |
| 2 | `interactive_mode_applies_configured_tool_output_mode` | CLI/TUI startup test closest to the wiring |
| 3 | `compact_mode_hides_successful_bash_body_but_keeps_title` | `anie-tui` replay/live transcript tests |
| 4 | `compact_mode_hides_successful_read_body_but_keeps_title` | same |
| 5 | `compact_mode_keeps_edit_diff_visible` | same |
| 6 | `compact_mode_keeps_write_result_visible` | same |
| 7 | `compact_mode_keeps_bash_and_read_errors_visible` | same |
| 8 | `slash_tool_output_reports_current_state` | `anie-tui/src/tests.rs` |
| 9 | `slash_tool_output_compact_is_ui_only` | same |
| 10 | `slash_tool_output_invalid_arg_is_rejected` | same / command validation tests |

## Risks

- **Data loss in the wrong layer:** if the mode is applied while
  building `ToolCallResult`, switching from `compact` back to `verbose`
  later in the session will not be able to recover hidden output.
- **Diff regression:** `edit` / `write` share the same tool-result
  rendering path, so the gate must be tool-name-specific.
- **Silent failures:** hiding failed `bash` / `read` output would remove
  the most actionable error information from the transcript.
- **Cache staleness:** changing the mode must invalidate `OutputPane`'s
  cached lines exactly like markdown toggles do.

## Exit criteria

- [ ] `UiConfig` supports `tool_output_mode = "verbose" | "compact"`.
- [ ] `verbose` preserves today's transcript behavior.
- [ ] `compact` hides successful `bash` and `read` result bodies in the
      interactive transcript.
- [ ] `edit` / `write` outputs remain visible in both modes, including
      diffs when present.
- [ ] failed `bash` / `read` calls still surface their error text.
- [ ] `/tool-output [verbose|compact]` works at runtime without
      dispatching controller work.

## Deferred

- Per-tool or per-call overrides.
- Extending compact mode to tools beyond `bash` / `read`.
- Making print mode configurable; it already behaves close to
  `compact`, so this can stay separate unless the user asks for
  cross-mode parity.
