# 07 — Input polish: one-line default, prefix color cue

## Rationale

Two findings folded together because they touch the same code
and ship as a single small PR.

### F-7: input box always reserves three lines

`crates/anie-tui/src/input.rs:267-272`:

```rust
pub fn preferred_height(&mut self, width: u16) -> u16 {
    let width = width.max(1);
    let cached = self.layout(width);
    let line_count = u16::try_from(cached.lines.len()).unwrap_or(u16::MAX);
    line_count.clamp(3, 8)
}
```

The minimum of 3 means the input box reserves three rows even
when empty. User report: "I would like for it to be a one-line
box until the user exceeds a one-line message. Then the box
should expand naturally."

This is a one-character clamp change.

### F-8: prefix color is static regardless of state

Anie's input box has no separate prompt prefix character today
(the `> ` is part of the layout string at
`crates/anie-tui/src/input.rs:502`). The border itself is
`Color::DarkGray`.

Codex (`codex-rs/tui/src/chat_composer.rs:3905-3923`) uses a
`›` prefix that changes color based on input state — cyan when
active, dim when disabled (e.g., during a pending tool call
where input is locked). Subtle visual feedback that the input
is responsive.

Anie has the lock state already (input is locked during agent
streaming + tool execution). Worth surfacing.

## Design

### Change 1: one-line default

```rust
pub fn preferred_height(&mut self, width: u16) -> u16 {
    let width = width.max(1);
    let cached = self.layout(width);
    let line_count = u16::try_from(cached.lines.len()).unwrap_or(u16::MAX);
    // Floor of 1 (input grows from a single row); ceiling of 8
    // unchanged. Rationale: reserve only as much as the buffer
    // needs, so empty/short prompts don't claim three rows.
    line_count.clamp(1, 8)
}
```

Plus border consideration: the input box has
`Borders::TOP | Borders::BOTTOM` (input.rs:305). With a 1-line
content area that's 3 rows total (top border + 1 content +
bottom border). User said keep the borders, so this is the
intended look.

Test the height computation, including the case where
content is 0 lines (empty buffer). `cached.lines` is at least
length 1 (`layout_lines_uncached` always pushes at least one
line). So `clamp(1, 8)` of 1 is 1, fine.

### Change 2: active/inactive prefix color cue

Anie's input layout currently uses `> ` as the textual prompt
in `layout_lines_uncached`. The prompt is part of the layout
string, not a separate Span — so we can't easily restyle it.

Two options:

**Option A: switch to a styled prefix Span at render time.**
Render the input as `[styled prefix] [content]` per line, with
the prefix span styled dynamically. Requires moving the prompt
out of the layout string.

**Option B: leave the prompt textual; restyle the Border instead.**
Currently `border_style: Style::default().fg(Color::DarkGray)`.
Change to:

```rust
let border_style = if input_locked {
    Style::default().add_modifier(Modifier::DIM)
} else {
    Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)
};
```

Cyan-dim border when input is active; plain dim when locked.

Option B is simpler and avoids changing the prompt-layout
contract. The visual cue is still present via the border
color shift.

Picking Option B for this PR.

The lock state is already known via `App::agent_state` — if
not Idle, input is locked. Pass a `bool input_locked` into
`InputPane::render`.

### Change 3: replace `> ` prompt with `›`

While we're here: change the prompt from `> ` (8 columns
including ANSI cursor offset) to `› ` (matches pi/codex
convention — Unicode chevron, 2 columns). Adjusts the
`prefix.len()` math in `layout_lines_uncached`.

This is in addition to the user's earlier change to `›` in
the user-message rendering (different code path).

Optional. Skip if it complicates the diff; the prefix swap is
nice-to-have, not in the user's complaint set.

## Files to touch

- `crates/anie-tui/src/input.rs` — `preferred_height` clamp,
  optional prompt char change.
- `crates/anie-tui/src/input.rs` — `render` accepts
  `input_locked: bool`, picks border style accordingly.
- `crates/anie-tui/src/app.rs` — pass `input_locked` to
  `input_pane.render`. Compute as
  `!matches!(self.agent_state, AgentUiState::Idle)`.
- Tests for the clamp, the locked-state border, optional
  prompt-char fixture update.

## Phased PRs

Single PR. All changes are small and share `input.rs`.

## Test plan

1. **`empty_input_box_prefers_one_line`** — fresh `InputPane`,
   width=80, `preferred_height` returns 1.
2. **`one_line_buffer_prefers_one_line`** — type "hello",
   `preferred_height` returns 1.
3. **`overflow_buffer_grows`** — type a buffer that wraps to 3
   lines at width=20, `preferred_height` returns 3.
4. **`grow_clamped_at_eight`** — type a buffer that would wrap
   to 12 lines, `preferred_height` returns 8.
5. **`render_uses_dim_border_when_locked`** — render with
   `input_locked=true`, scan the buffer for `DIM` modifier on
   the border row.
6. **`render_uses_cyan_border_when_active`** — render with
   `input_locked=false`, scan for `Cyan` foreground.
7. **Manual smoke**: open anie, observe 1-row input on launch.
   Type a long message, observe the input grow line by line.

## Risks

- **Three-row floor was protecting users from too-small input
  on terminal resize.** Verify the resize path handles 1-row
  input (the App::render layout reserves space top-down;
  `preferred_height` = 1 means 1 content row + 2 border rows =
  3 actual rows). Should be fine.
- **Cyan border on light terminals.** Cyan tends to render
  well on both light and dark, but check during smoke.
- **`›` prompt change breaks layout fixtures.** If we pick the
  prefix change, expect snapshot test churn. Defer if the
  diff bloats.

## Exit criteria

- All 7 tests above pass.
- Manual smoke confirms 1-row default + natural growth.
- Border color reflects locked vs active state.
- `cargo test --workspace` green; clippy clean.
- No bench regression (input pane renders are cached after PR
  01 of perf round, so this is a no-cost change).

## Deferred

- Submitting on `Ctrl+Enter` instead of plain `Enter` (some
  apps prefer this so multi-line is the default). Anie's
  current Enter-submits / Shift+Enter-newline matches pi and
  codex; no change.
- Cursor styling (block vs bar vs underline). Terminal-level
  configuration.
- Prompt character change. Optional addition, see above.
- Any other "input feels slow" investigation. The user
  acknowledged the lag wasn't quite gone but couldn't point
  to specifics. Reopen with a concrete example.
