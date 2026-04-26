# 03 — Palette + borders: `.dim()` and rounded corners

## Rationale

Two related fixes that are both mechanical and yield an
immediate visual upgrade across the whole TUI.

### F-3: `Color::DarkGray` is fixed-color, brittle

39 sites in `crates/anie-tui/src/` use `Color::DarkGray` for
muted/secondary text — status bar, blockquote gutter, link
URL, autocomplete description, input border, etc.

`DarkGray` is a *concrete ANSI color* (color index 8 / "bright
black"). On most dark terminal palettes it's about RGB(85,85,85).
On light terminals it can render nearly invisible because the
"bright black" slot is often very close to the background.
Different terminal themes also remap index 8 to wildly
different shades.

Codex (`codex-rs/tui/src/`) uses the `.dim()` modifier
(`Modifier::DIM`) for the same purpose. `.dim()` is a *style
modifier* the terminal applies to whatever the current
foreground color is. Result: muted text stays legible against
whatever background the user has set, light or dark.

Pi uses a similar approach via its theme functions.

### F-4: Sharp borders look dated

Anie's blocks use ratatui's default border type — sharp angles
(`┌─┐│└─┘`). Codex uses `BorderType::Rounded` (`╭─╮│╰─╯`)
consistently
(`codex-rs/tui/src/onboarding/auth.rs:612-616` and elsewhere).
Pi uses Unicode box-drawing characters of various weights.

The change is one extra method call per `Block::default()...`
chain. ratatui handles it natively.

## Design

Two passes:

### Pass A: replace `Color::DarkGray` with `.dim()` (or appropriate fg + dim)

For each `Color::DarkGray` site, decide whether the intent was:
1. **Muted text on top of default fg** → switch to
   `Style::default().add_modifier(Modifier::DIM)` (drop the
   color, keep the dim modifier).
2. **Specific dim color for a colored element** (e.g., a green
   bullet that should look muted) → use the colored fg + DIM
   modifier together.

Audit each site:
- `crates/anie-tui/src/input.rs:307` — input border style →
  `Style::default().add_modifier(Modifier::DIM)`.
- `crates/anie-tui/src/markdown/theme.rs:link_url` — fallback
  URL text → `.dim()`.
- `crates/anie-tui/src/markdown/theme.rs:blockquote_gutter` —
  `│` gutter → `.dim()`.
- Status bar text in `app.rs:render_status_bar` → keep in mind
  the user said "status bar is OK"; only switch DarkGray sites,
  preserve other styling.
- Bullet/box helpers in `output.rs:format_tool_header_spans`,
  `boxed_lines`, `prefix_lines` — review color intent per call.

This is a mechanical sweep. Plan to do all 39 sites in one
pass to avoid leaving the TUI half-converted.

### Pass B: rounded borders

For each `Block::default().borders(...)` call, append
`.border_type(BorderType::Rounded)` (or move to a shared
helper).

Sites:
- `crates/anie-tui/src/input.rs:305` — input box (Borders::TOP|BOTTOM)
- `crates/anie-tui/src/autocomplete/popup.rs:155` — popup
- `crates/anie-tui/src/overlays/model_picker.rs:120` — picker
- `crates/anie-tui/src/overlays/providers.rs:236, :705`
- `crates/anie-tui/src/widgets/panel.rs:56`
- `crates/anie-tui/src/overlays/onboarding.rs:324, :1419, :1489`

Edge case: `Borders::TOP|BOTTOM` (input box) doesn't have
corners — only horizontal lines. `BorderType::Rounded` doesn't
change anything for this case. Apply anyway for consistency
(it's a no-op there).

The user explicitly said "I actually like the border lines"
for the input box. Confirmed: we're keeping the borders, just
making them rounded where they wrap a box (overlays, popups).
Input box stays top+bottom horizontal lines.

### Inverted-tilde / box-drawing fallback considerations

Some terminals (very old, or restricted to Latin-1) can't
render `╭`. In practice every terminal anie supports renders
the rounded characters fine (kitty, iterm2, gnome-terminal,
windows terminal, vscode, alacritty, ghostty, wezterm). No
fallback needed; if anyone reports issues, add a config knob.

## Files to touch

Pass A (palette):
- `crates/anie-tui/src/input.rs`
- `crates/anie-tui/src/markdown/theme.rs`
- `crates/anie-tui/src/output.rs` (multiple sites in tool
  helpers)
- `crates/anie-tui/src/app.rs` (status bar)
- `crates/anie-tui/src/autocomplete/popup.rs`
- `crates/anie-tui/src/overlays/*.rs`
- `crates/anie-tui/src/widgets/panel.rs`

Pass B (borders):
- Same files as Pass A wherever `Block::default()` constructs
  bordered blocks.

Tests:
- Existing snapshot tests in `tests.rs` capture rendered output;
  they'll need updates after the pass since visible characters
  change. Use that as the verification.

## Phased PRs

One PR, two commits. Splitting palette and borders is awkward
since they touch the same files often.

If reviewers push back on the size, split:
- 03a: palette sweep (`Color::DarkGray` → `.dim()`)
- 03b: rounded borders

## Test plan

1. **Existing snapshot tests** — update snapshots to reflect
   `╭`/`─`/`╮`/`│`/`╰`/`╯` borders and the absence of `DarkGray`
   color codes. Diff review confirms intent.
2. **`darkgray_does_not_appear_in_default_render`** — render
   a representative transcript, scan the output buffer for
   `DarkGray`-colored cells. Assert none, or only at sites
   we deliberately kept (if any).
3. **Manual smoke on a light terminal** — open anie under a
   light terminal theme, verify status bar / hints / muted
   text are legible. (Manual, document in commit message.)
4. **Manual smoke on a dark terminal** — same, verify nothing
   is *too* dim now (over-correction risk).
5. **Manual smoke under tmux** — rounded borders should still
   render; tmux passes through Unicode by default.

## Risks

- **`.dim()` rendering varies by terminal.** Most modern
  terminals respect SGR 2 (faint/dim). Some don't, in which
  case the muted text renders at full intensity (still
  legible, just not muted). Acceptable.
- **Snapshot test sprawl.** Every test that asserts on the
  rendered buffer will see the new characters. Plan time for
  reviewing diffs across the test suite.
- **Borders::TOP|BOTTOM no-op for rounded.** As noted.

## Exit criteria

- All `Color::DarkGray` usages converted (or explicitly
  documented as kept).
- All bordered Blocks use `BorderType::Rounded`.
- Snapshot tests updated and reviewed.
- Manual smoke on light + dark terminals passes.
- `cargo test --workspace` green; clippy clean.

## Deferred

- A full theme system with named palettes (light theme,
  high-contrast, etc.). Out of scope; this PR is a
  step toward making that easier later, not a substitute.
- Rounded corners on the input box's horizontal-only
  borders. There aren't any corners to round there;
  `Borders::TOP|BOTTOM` skips the corner positions.
