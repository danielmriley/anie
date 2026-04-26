# TUI polish — 2026-04-26

User report after the perf round (`docs/tui_perf_2026-04-25/`):
> "The markdown we render seems incorrect. Lists give a lot of
> space between them, the agent prints the raw markdown and then
> when a line is done it is converted and the visible text
> changes. I don't like that. The TUI looks ok for the most part
> but there are some things that feel… cheap?"

This round is UX, not perf. The 25–62% perf wins from the
previous round are in main; latency is fine. What remains is
**how the output looks and feels** — markdown rendering
correctness, palette legibility, border style, and a couple of
input-box tweaks.

Codex (`/home/daniel/Projects/agents/codex/codex-rs/`) was the
primary reference for this round, alongside pi. Both render
markdown for streaming content without the "raw text → snap to
styled" transition anie has today; both use `.dim()` modifier
instead of fixed-color `DarkGray`; codex uses rounded borders
and an adaptive background tint for user messages.

## Files in this folder

- [`00_report.md`](00_report.md) — findings recap with file:line
  citations to anie, pi, and codex.
- [`01_streaming_markdown.md`](01_streaming_markdown.md) — drop
  the tail-as-plain commit-boundary logic; render full
  accumulated text as markdown each frame.
- [`02_list_spacing.md`](02_list_spacing.md) — honor pulldown-
  cmark's tight-vs-loose list distinction so tight lists render
  compact instead of double-spaced.
- [`03_palette_and_borders.md`](03_palette_and_borders.md) —
  swap `Color::DarkGray` → `.dim()` modifier across the TUI;
  switch `Borders::ALL` blocks to `BorderType::Rounded`.
- [`04_user_message_tint.md`](04_user_message_tint.md) — adopt
  codex's adaptive background tint on user messages (light/dark
  terminal aware).
- [`05_spinner.md`](05_spinner.md) — replace the braille spinner
  in the activity row with codex's bullet-shimmer / blink
  fallback.
- [`06_layout_simplification.md`](06_layout_simplification.md)
  — sweep after 01 lands; delete the tail-as-plain machinery,
  simplify table layout. Target ~1,200 LOC in `layout.rs` (down
  from 1,866).
- [`07_input_polish.md`](07_input_polish.md) — input box
  defaults to one line and grows as content overflows; active-
  vs-disabled prefix color cue.

## Suggested PR ordering

1. **PR 03** (palette + borders) — mechanical, low risk,
   immediate visual win across the whole TUI. Should land first
   so subsequent PRs render against the new look.
2. **PR 02** (list spacing) — small focused fix for a concrete
   complaint.
3. **PR 07** (input polish) — one-line clamp change + small
   prefix color addition. Low risk.
4. **PR 01** (streaming markdown) — the big UX win. Bench-
   verified against the existing keystroke / streaming benches
   so we don't regress the perf round.
5. **PR 06** (layout simplification) — cleanup that becomes
   natural once PR 01's commit-boundary code is dead.
6. **PR 04** (user message tint) — terminal background
   detection has corner cases; land after the structural pieces
   are in place.
7. **PR 05** (spinner) — subjective; iterate. Optional.

## Principles

- **Match pi's / codex's shape unless we have a documented
  reason to deviate.** Both render markdown for streaming, both
  use `.dim()`, codex uses rounded borders and tinted user
  messages. Anie is the outlier in each case; the changes here
  reduce that gap.
- **No new features.** This is polish, not feature work. Every
  plan describes a behavior that already exists in pi or codex.
- **Don't regress the perf round.** Each plan that touches the
  render hot path (PR 01 in particular) lists the existing
  `tui_render` benches it must clear before merging.

## Out of scope

- Theme picker / light theme / `/theme` slash command. Useful
  follow-up; deliberately deferred until palette is fixed
  enough that a dark-only deployment looks decent.
- Settings overlay (deferred from the previous round).
- Full alternate-screen → scrollback architectural pivot
  (different product; not on the table).
- Image protocol support (Kitty/iTerm2 inline images). Pi
  has it; anie doesn't. Big surface, no user pain reported.
