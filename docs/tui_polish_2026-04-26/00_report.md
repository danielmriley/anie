# 00 — TUI polish investigation report

Date: 2026-04-26
Branch: `feat/tui-polish` from `main` at `49de66b`
Method: side-by-side comparison of anie / pi
(`/home/daniel/Projects/agents/pi/`) / codex
(`/home/daniel/Projects/agents/codex/codex-rs/`).

## Pain points (verbatim from user)

> 1. "The agent prints the raw markdown and then when a line is
>    done it is converted and the visible text changes."
> 2. "Lists give a lot of space between them."
> 3. "The TUI looks ok for the most part but there are some
>    things that feel… cheap."
> 4. "Make the input box one-line and expand naturally when the
>    user exceeds one line of content."
> 5. (4-A) "Rounded borders sound nice."
> 6. (4-B) "Color palette could be brightened up ever so
>    slightly."
> 7. (4-C) "Highlight user messages similarly to pi and codex."
> 8. (4-D) "The spinner needs work — but I'm not sure why."

## Findings

### F-1. Tail-as-plain streaming render (the "raw markdown"
issue)

`crates/anie-tui/src/output.rs:80-148`. `StreamingAssistantRender`
splits accumulated text at the last `\n\n` boundary outside a
code fence. Committed prefix renders as full markdown
(cached); tail renders as plain wrapped text via `wrap_text`.

Effect: `**bold**` shows as literal asterisks until a blank
line arrives, at which point the tail commits and re-renders
as styled bold. The user sees text "snap" between styles each
time a paragraph completes.

Pi and codex both render the entire accumulated text as
markdown on every update:
- Pi (`packages/coding-agent/src/modes/interactive/interactive-mode.ts:2727`):
  `updateContent(message)` re-renders the full message on each
  delta.
- Codex (`codex-rs/tui/src/streaming/commit_tick.rs`): commit-
  tick architecture re-renders queued lines as fresh
  `HistoryCell`s; no partial-vs-full distinction.

The original perf reason for anie's tail-as-plain is largely
gone after PR 06 (`Arc<Line>` sharing) — re-parsing markdown of
a streaming buffer up to ~10 KB is sub-millisecond. Above that,
fall back is acceptable.

**Severity: confirmed-hot UX.** Addressed by PR 01.

### F-2. List items separated by blank lines

`crates/anie-tui/src/markdown/layout.rs` paragraph-end handler
emits a blank line for every Paragraph end event, including
ones inside list items. pulldown-cmark's distinction between
"tight" lists (no Paragraph wrapping for items) and "loose"
lists (Paragraph wrapping) is not honored.

Pi tracks list looseness explicitly
(`packages/tui/src/components/markdown.ts:546-611` — list
items are concatenated without intermediate blank lines unless
the source has them). Codex follows the same rule via its
markdown renderer.

**Severity: confirmed UX.** Addressed by PR 02.

### F-3. `Color::DarkGray` for muted text

`crates/anie-tui/src/` has 39 `Color::DarkGray` sites — status
bar, input prefix, blockquote gutter, link URL, autocomplete
hint, spinner row, etc. `DarkGray` is a *fixed* ANSI color.
On light terminals it can appear nearly invisible; on dark
terminals with custom palettes it may render as something
unintended.

Codex uses `.dim()` (`Modifier::DIM`) throughout for the same
purpose
(`codex-rs/tui/src/history_cell.rs:438`, etc.). `.dim()` is a
*modifier* the terminal applies to whatever the current
foreground color is — adaptively legible on both light and
dark backgrounds.

**Severity: confirmed UX (legibility on light terminals).**
Addressed by PR 03.

### F-4. Sharp borders

Anie uses default `Borders::ALL` / `Borders::TOP|BOTTOM`
(sharp characters: `┌─┐│└─┘`). Found in:
- `crates/anie-tui/src/input.rs:305` — input box (top/bottom)
- `crates/anie-tui/src/autocomplete/popup.rs:155`
- `crates/anie-tui/src/overlays/model_picker.rs:120`
- `crates/anie-tui/src/overlays/providers.rs:236, :705`
- `crates/anie-tui/src/widgets/panel.rs:56`
- `crates/anie-tui/src/overlays/onboarding.rs:324, :1419, :1489`

Codex uses `BorderType::Rounded` (`╭─╮│╰─╯`) consistently
(`codex-rs/tui/src/onboarding/auth.rs:612-616`).

**Severity: code-health / aesthetic.** Addressed by PR 03.

### F-5. User messages have no background highlighting

`crates/anie-tui/src/output.rs:986-996` (`block_lines` UserMessage
branch). User messages render as bold-dim cyan `›` prefix +
plain raw text. No background tint, no left margin, no other
visual distinction.

Codex (`codex-rs/tui/src/style.rs:17-38`,
`codex-rs/tui/src/history_cell.rs:381-385`) applies an
*adaptive background tint*:
- 4 % black blend on light terminals
- 12 % white blend on dark terminals
- Detected via terminal-bg query (OSC 11) with a config
  fallback.

The tint, plus a `›` prefix and top/bottom blank lines for
breathing room, is enough to differentiate user turns at a
glance without a heavy left border.

Pi's user messages also get a background tint (slightly
different shade, same idea).

**Severity: confirmed UX.** Addressed by PR 04.

### F-6. Spinner glyph

`crates/anie-tui/src/app.rs:2151` (Streaming arm of
`render_spinner_row`): `format!("{spinner_frame} Responding...")`
where `spinner_frame` rotates through Braille dots
(`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`).

Codex's strategy
(`codex-rs/tui/src/exec_cell/render.rs:183-197`): bullet `•`
that *shimmers* on 24-bit-color terminals (color cycles through
shades), fallback to *blink* (`•` ↔ `◦`, 600 ms period) on
16-color terminals. Reserves Braille for terminal title only.

User said "the spinner needs work, but I'm not sure why."
Likely candidates:
- Braille glyphs render as rectangular blobs in some monospace
  fonts.
- Animation cadence (anie ticks at ~10 fps) feels sluggish vs.
  codex's continuous shimmer.
- "Responding..." string (with three dots) is itself static
  noise; the spinner is the live indicator.

**Severity: subjective.** Addressed by PR 05 (optional).

### F-7. Input box is always ≥ 3 lines tall

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
when empty. User wants it to start at one line and grow as
content overflows, up to the existing 8-line cap.

Codex's input is also one-line-by-default with grow-on-overflow
(`codex-rs/tui/src/chat_composer.rs`).

**Severity: explicit user request.** Addressed by PR 07.

### F-8. Input-prefix color is static

`crates/anie-tui/src/input.rs:305-308` — input box border style
is `Color::DarkGray` regardless of agent state. Anie has no
prompt-prefix character today (the `>` is part of the layout
string at `input.rs:502`).

Codex
(`codex-rs/tui/src/chat_composer.rs:3905-3923`) uses a `›`
prefix that changes color based on input state: cyan when
active, dim when disabled (e.g., during a pending tool call
where input is locked).

**Severity: subjective polish.** Folded into PR 07.

### F-9. Anie's markdown layout is ~3× pi's for the same
features

`crates/anie-tui/src/markdown/layout.rs` is 1,866 LOC. Pi's
`packages/tui/src/components/markdown.ts` is 852 LOC. Same
visible feature set: headings, bold/italic, inline code, code
blocks with syntax highlighting, lists, blockquotes, tables,
links.

Anie's layout engine has explicit table-column negotiation,
nested-list state machine, and several helper passes that pi
gets for free by walking the marked token stream and emitting
styled lines.

After PR 01 lands (which makes the tail-as-plain branch dead
code), the natural follow-up is a sweep to delete dead
machinery and simplify table layout. Target ~1,200 LOC, same
feature set.

**Severity: code-health.** Addressed by PR 06.

## Mapping pain → plans

| User pain | Plan |
|-----------|------|
| Raw markdown until newline (#1) | PR 01 |
| List spacing (#2) | PR 02 |
| Cheap feel — palette (#4-B) | PR 03 (palette half) |
| Cheap feel — borders (#4-A) | PR 03 (borders half) |
| Cheap feel — user message highlight (#4-C) | PR 04 |
| Cheap feel — spinner (#4-D) | PR 05 |
| Code is large (#3) | PR 06 |
| Input box always 3 lines (#5) | PR 07 |

## Why now (and not earlier)

The perf round (`docs/tui_perf_2026-04-25/`) took anie from
felt-slow to bench-verified-fast. With latency no longer the
gating issue, what remains is visual quality — exactly the
class of issue that's hardest to spot in a benchmark but most
important to a user actively typing. This round is the next
layer of the same review.
