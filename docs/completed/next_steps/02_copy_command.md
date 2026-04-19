# 02 — `/copy` Command

## Goal

Add a `/copy` command that copies the last assistant response text to the
system clipboard.

## Current behavior

No clipboard integration exists. Users must manually select and copy text.

## Change

### Implementation

1. Add `arboard` as a dependency to `anie-tui` for cross-platform clipboard.

2. Handle `/copy` in `App::handle_slash_command()`:
   - Find the last `RenderedBlock::AssistantMessage` in the output pane
   - Extract its `text` field (visible answer, not thinking)
   - Copy to clipboard via `arboard::Clipboard`
   - Show a system message confirming the copy (or error)

3. Add `/copy` to the help text.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/Cargo.toml` | Add `arboard` dependency |
| `crates/anie-tui/src/app.rs` | Handle `/copy` in `handle_slash_command()`, update help text |

### Tests

1. Unit test: `/copy` with no assistant messages shows an error
2. Unit test: `/copy` with assistant messages extracts the correct text
   (Note: clipboard operations may not work in headless CI — test the
   text extraction logic, not the clipboard write itself)

### Exit criteria

- [x] `/copy` copies the last assistant answer to clipboard
- [x] Thinking text is not included in the copy
- [x] Error message shown when no assistant messages exist
- [x] Help text includes `/copy`
- [x] All existing tests pass
