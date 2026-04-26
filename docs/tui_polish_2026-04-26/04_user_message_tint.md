# 04 — User message tint: adopt codex's adaptive background

## Rationale

Finding F-5. Today user messages render as a bold-dim cyan `›`
prefix on a default background, with no other visual
distinction from assistant text. They're easy to skim past in
a long transcript, especially on a dark theme where the cyan
prefix is the only marker.

Pi and codex both apply a *background tint* to user messages.
Codex's implementation
(`codex-rs/tui/src/style.rs:17-38`) is the reference:

```rust
pub fn user_message_style_for(terminal_bg: Option<(u8, u8, u8)>) -> Style {
    match terminal_bg {
        Some(bg) => Style::default().bg(user_message_bg(bg)),
        None => Style::default(),
    }
}

fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    // Adaptive perceptual blend:
    // 4 % black on light terminals, 12 % white on dark.
    ...
}
```

The tint is subtle (a few percent toward black or white,
depending on perceived terminal luminance), so user messages
are *visible at a glance* without becoming a heavy banner.

User report: "I'd like the user messages to be highlighted
similarly to pi and codex. All of the coding agent tools do
that from what I have seen."

## Design

### Step 1: terminal background detection

Codex queries OSC 11 (`\x1b]11;?\x07`) on startup to ask the
terminal for its background color. Most modern terminals
respond with `rgb:RRRR/GGGG/BBBB`. From that we know whether
to lean light or dark.

Reuse anie's existing `TerminalCapabilities` shape (mirrors
pi's). Add a `background_color: Option<(u8, u8, u8)>` field.
Detection sources, in order of preference:
1. **OSC 11 query** at startup, with a 100 ms timeout. If a
   response arrives, use it.
2. **`COLORFGBG` env var** (set by some terminals like xterm,
   urxvt). Parse the second component (background).
3. **Heuristic from `$COLORTERM` or terminal-program env vars
   that imply dark themes** (e.g., the default for kitty,
   iterm2, alacritty). Default-to-dark when unknown.
4. **Config override**: `[ui] terminal_background = light|dark`
   in `anie.toml` for users on terminals that don't respond.

The existing `TerminalCapabilities::detect_from(env)` already
supports env-based detection; OSC 11 query needs new code.

### Step 2: compute tint color

Given the detected background:

```rust
fn user_message_bg(terminal_bg: (u8, u8, u8)) -> Color {
    let luminance = perceived_luminance(terminal_bg);
    if luminance > 0.5 {
        // Light terminal — blend 4 % toward black.
        blend(terminal_bg, (0, 0, 0), 0.04)
    } else {
        // Dark terminal — blend 12 % toward white.
        blend(terminal_bg, (255, 255, 255), 0.12)
    }
}
```

`perceived_luminance` is the standard 0.299·R + 0.587·G +
0.114·B / 255 formula.

Output is a `Color::Rgb(r, g, b)` — works on 24-bit terminals.
On 16-color or 256-color terminals, the blend produces a
specific RGB that ratatui will downgrade or render literally
(depending on backend). Worst case it falls back to no tint
and we show the message with just the prefix — same as today.

### Step 3: apply to user-message rendering

In `crates/anie-tui/src/output.rs:986-996` (`block_lines`
UserMessage branch), wrap the rendered lines in a Block with
the background style, OR set the style on each Line/Span. The
latter is simpler and avoids a full Block widget.

Looking at codex's
`/codex-rs/tui/src/history_cell.rs:381-385`:

```rust
lines.extend(prefix_lines(
    wrapped_message,
    "› ".bold().dim(),
    "  ".into(),
));
```

Prefix lines wrap the user text with `› ` on the first line
and 2-space indent on subsequent lines. The tint is applied at
a higher level via the cell's display style.

Anie's approach: apply the tint style to each Span of each
Line in the user-message block:

```rust
fn apply_tint(line: Line<'static>, tint: Style) -> Line<'static> {
    let spans = line.spans.into_iter().map(|s| Span {
        style: s.style.patch(tint),
        ..s
    }).collect();
    Line { spans, ..line }
}
```

Then add a top blank line and bottom blank line for breathing
room (codex pattern at `history_cell.rs:367, 388`).

## Files to touch

- `crates/anie-tui/src/terminal_capabilities.rs` — add
  `background_color: Option<(u8, u8, u8)>`; OSC 11 query
  helper.
- `crates/anie-tui/src/terminal.rs` — call OSC 11 query as part
  of setup, populate `TerminalCapabilities`.
- `crates/anie-tui/src/output.rs` — `RenderContext` carries the
  tint style (or the detected bg); `block_lines` UserMessage
  branch applies it.
- `crates/anie-config/src/lib.rs` — `[ui] terminal_background`
  config knob.
- New module or file with the blend / luminance math, kept
  small and testable.
- Tests for blend math, luminance detection, OSC 11 parser.

## Phased PRs

Suggested split:

- **04a — config + tint application without detection.**
  Hard-code the dark-terminal tint (12 % white blend). Apply
  to user messages. Lets us validate the visual without OSC 11
  complexity.
- **04b — OSC 11 query + adaptive selection.** Add detection,
  switch tint based on detected background.

If 04a feels good and we don't get reports from light-terminal
users, 04b can be deferred indefinitely. Default-to-dark is the
common case.

## Test plan

1. **`luminance_of_pure_black_is_zero`**, **`_white_is_one`** —
   sanity checks on the formula.
2. **`luminance_threshold_classifies_typical_terminals`** — a
   table of common terminal default backgrounds (Solarized Dark,
   GitHub Light, etc.) classifies correctly.
3. **`blend_produces_expected_intermediates`** — blend(black,
   white, 0.5) ≈ (127, 127, 127); blend(black, white, 0.0) =
   black.
4. **`user_message_tint_applied_to_each_line`** — render a
   user message under a non-default `RenderContext` tint;
   assert every Span on every line carries the tint bg.
5. **`osc11_parser_accepts_well_formed_response`** — parse
   `"\x1b]11;rgb:1234/5678/9abc\x07"` → `(0x12, 0x56, 0x9a)`
   (high byte of each component).
6. **`osc11_parser_rejects_garbage`** — defensive parse for
   malformed responses; returns `None`.
7. **Manual smoke**: light terminal, dark terminal, terminal
   that ignores OSC 11 (config fallback).

## Risks

- **OSC 11 query blocks startup.** Mitigate with a 100 ms
  timeout. Also: only query once at startup; cache the result.
- **Some terminals echo OSC 11 query as text** if they don't
  recognize it. Anie reads stdin during startup; need to drain
  the response without polluting the input pane. Check codex's
  implementation pattern.
- **`Color::Rgb` falls back to nearest 256-color or 16-color**
  on terminals without truecolor. The blended tint may
  collapse to a recognizable adjacent color. Acceptable.
- **The blend math is taste-dependent.** Codex's 4 % / 12 %
  ratio is a reasonable starting point. Be prepared to adjust
  after manual smoke.

## Exit criteria

- User messages render with a visible (but subtle) background
  tint distinguishing them from assistant turns.
- The tint adapts to detected terminal background, OR we ship
  04a only with dark-terminal default and a config override.
- Manual smoke on light + dark terminals confirms readability
  and aesthetic improvement.
- `cargo test --workspace` green; clippy clean.
- No bench regression.

## Deferred

- Background tint for assistant messages, system messages,
  tool blocks. Keep their default backgrounds; the user-
  message tint is the differentiator.
- Animated tint (fade-in on a new user turn). Static is fine.
- Per-theme tint overrides (`[ui.theme.dark] user_msg_tint = ...`).
  Out of scope; the adaptive formula is meant to be theme-
  agnostic.
