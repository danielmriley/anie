# Step 0 — TUI Transcript Scrolling and Navigation

This step improves transcript navigation so long assistant output is actually usable in the TUI.

It should land immediately after the OpenAI-compatible hotfix step and before the broader local-reasoning work.

## Why this step exists

Longer reasoning output will make transcript navigation much more important.

Even before the local-reasoning feature expansion, the current user complaint is real:
- the user cannot reliably scroll up to older history
- the user cannot reliably inspect the beginning of a long wrapped message
- mouse capture is enabled, but mouse-wheel transcript scrolling is not actually handled

This means usability is currently behind where the transcript renderer already wants to go.

---

## Primary outcomes required from this step

By the end of this step:
- users can scroll transcript history up and down predictably
- users can jump to the top and bottom of the transcript
- users can use the mouse wheel for transcript navigation
- auto-follow remains correct while streaming
- long wrapped messages are explicitly test-covered

---

## Current code facts

### Already present

In `crates/anie-tui/src/output.rs`:
- `OutputPane` already stores:
  - `scroll_offset`
  - `auto_scroll`
  - `last_total_lines`
  - `last_viewport_height`
- `scroll_up(...)` and `scroll_down(...)` already exist
- `render(...)` already computes `max_scroll()` and applies `Paragraph::scroll(...)`

In `crates/anie-tui/src/app.rs`:
- `PageUp` and `PageDown` are already wired in both idle and active states
- `last_output_height` is already tracked

In `crates/anie-tui/src/terminal.rs`:
- mouse capture is already enabled

In `crates/anie-tui/src/tests.rs`:
- there is already a test for page scrolling over many short messages

### Still missing / insufficient

- no `Home` / `End` handling
- no mouse-wheel handling in `App::handle_terminal_event(...)`
- no explicit test for a single long wrapped assistant message
- no user-facing hint that the viewport is scrolled away from the bottom
- scroll actions are still emitted upward as `UiAction::ScrollUp/ScrollDown`, even though the controller ignores them

---

## Files expected to change

Primary:
- `crates/anie-tui/src/app.rs`
- `crates/anie-tui/src/output.rs`
- `crates/anie-tui/src/tests.rs`

Possible cleanup:
- `crates/anie-cli/src/controller.rs`
- `crates/anie-tui/src/lib.rs`

---

## Constraints

1. Keep scrolling state inside `anie-tui`.
2. Do not persist scroll state.
3. Do not add controller/session complexity for transcript navigation.
4. Preserve existing input editing behavior.
5. Scrolling must operate over rendered lines, not just message boundaries.

---

## Recommended implementation order inside this step

### Sub-step A — make `OutputPane` navigation API explicit

Clarify the `OutputPane` API around transcript navigation.

Even if the internal state continues to use line offsets, add or refine helpers so the public behavior is clear:
- `scroll_line_up(...)`
- `scroll_line_down(...)`
- `scroll_page_up(...)`
- `scroll_page_down(...)`
- `scroll_to_top()`
- `scroll_to_bottom()`
- `is_at_bottom()` / `is_scrolled()`

The goal is to make later app-level event handling readable instead of embedding all semantics in `scroll_up(...)` / `scroll_down(...)` alone.

### Sub-step B — add top/bottom navigation bindings

In `crates/anie-tui/src/app.rs`, add:
- `Home` → jump to top of transcript
- `End` → jump to bottom and re-enable auto-follow

Keep these available in both idle and active states, just like `PageUp` / `PageDown`.

### Sub-step C — add mouse-wheel transcript scrolling

Extend `App::handle_terminal_event(...)` to handle mouse events.

At minimum support:
- wheel up → scroll transcript upward by a small number of lines
- wheel down → scroll transcript downward by a small number of lines

This can remain a pure TUI action. The controller does not need to know.

### Sub-step D — preserve auto-follow semantics deliberately

Audit the interplay between:
- `OutputPane::render(...)`
- transcript mutation methods like `add_block(...)`, `append_to_last_assistant(...)`, `finalize_last_assistant(...)`
- `TranscriptReplace`
- scroll navigation methods

Required behavior:
- if the user is at bottom, new content follows automatically
- if the user scrolls up, new content does not snap the viewport back down
- `End` restores bottom-follow explicitly
- transcript replacement does not leave the pane in a broken or surprising state

### Sub-step E — add a visible “scrolled away from bottom” hint

Pick a minimal first indicator.

Good options:
- a status-bar marker such as `↑ history`
- or a small output-pane footer marker if that is cleaner visually

The important requirement is discoverability: users should understand why new output is not auto-following.

### Sub-step F — decide whether to stop sending no-op scroll actions to the controller

Today `App` sends `UiAction::ScrollUp` / `ScrollDown`, but `crates/anie-cli/src/controller.rs` ignores them.

Preferred end state:
- transcript scrolling remains entirely local
- no controller event is emitted for a pure viewport change

This cleanup is optional if it would create unnecessary churn right now, but it should at least be evaluated in this step.

### Sub-step G — add long-message navigation tests

Create tests that exercise:
- one long wrapped assistant message spanning many viewport heights
- the ability to scroll to the top of that one message
- returning to the bottom afterward

This is a distinct case from many short transcript blocks and must be covered directly.

---

## Detailed code touchpoints

### `crates/anie-tui/src/output.rs`

Likely updates:
- explicit navigation helpers
- optional status/query helpers like `is_at_bottom()` / `is_scrolled()`
- careful handling of `auto_scroll` in relation to top/bottom jumps

### `crates/anie-tui/src/app.rs`

Likely updates:
- `handle_terminal_event(...)` to include mouse events
- `handle_idle_key(...)` and `handle_active_key(...)` for `Home` / `End`
- possible UI hint plumbing for scrolled-away-from-bottom state
- optional removal of no-op `UiAction::ScrollUp/ScrollDown`

### `crates/anie-cli/src/controller.rs`

Only if cleanup is chosen:
- remove no-op handling for `UiAction::ScrollUp` / `UiAction::ScrollDown`
- or leave it intentionally as a no-op with a comment if cleanup is deferred

---

## Test plan

### Required TUI tests

1. **page scrolling over many short messages**
   - keep the existing test and update if behavior changes

2. **jump to top / bottom works**
   - `Home` reaches earliest visible content
   - `End` returns to latest visible content and re-enables auto-follow

3. **mouse-wheel scroll works**
   - wheel-up scrolls history upward
   - wheel-down scrolls downward

4. **single long wrapped assistant message is navigable**
   - one message spans multiple pages
   - top of message becomes reachable

5. **auto-follow remains off while scrolled away from bottom**
   - new transcript content does not yank the viewport

6. **auto-follow resumes at bottom**
   - after `End` or enough downward scrolling, new content follows again

7. **transcript replacement remains sane**
   - especially after `TranscriptReplace`

### Optional cleanup tests

If scroll actions are removed from the controller path:
- ensure nothing in controller tests depends on them

---

## Manual validation plan

1. Start the TUI with enough history to exceed the viewport.
2. Use `PageUp` and confirm older content appears.
3. Use the mouse wheel and confirm transcript navigation works.
4. Press `Home` to jump to the earliest transcript content.
5. Press `End` to return to the latest content.
6. Start streaming output while scrolled up and confirm the viewport does not snap.
7. Return to the bottom and confirm streaming auto-follow resumes.
8. Test with a single very long assistant message and verify the beginning is reachable.

---

## Risks to watch

1. **input/editor interference**
   - new navigation bindings must not break typing or slash-command behavior
2. **mouse-event portability**
   - mouse-wheel event shapes can vary slightly across terminals; keep handling simple and tolerant
3. **hidden auto-follow state**
   - without a visual hint, users may think scrolling is broken when follow is intentionally disabled
4. **controller/TUI boundary drift**
   - keep viewport logic local even if older actions still exist

---

## Exit criteria

This step is complete only when all of the following are true:
- transcript scrolling works predictably by keyboard and mouse
- `Home` / `End` transcript navigation works
- long wrapped assistant messages are explicitly test-covered
- auto-follow semantics remain correct while streaming
- scroll behavior is clearly TUI-owned

---

## Follow-on step

After this step is green, proceed to:
- `step_1_openai_system_prompt_insertion_point.md`
