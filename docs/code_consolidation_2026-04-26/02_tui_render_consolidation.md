# 02 — TUI render consolidation

Medium-risk consolidations across the non-markdown TUI render
code. Not implemented in this branch — wants careful review
because each touches user-visible rendering paths.

## Items

### F-TUI-1: bullet-header builder

Three header-building functions share near-identical patterns:
- `output.rs:1602` `format_tool_header_spans`
- `output.rs:1314` `assistant_thinking_lines` header
- `output.rs:1281` `assistant_error_lines` header

Each builds: bullet (colored, sometimes with spinner) → label
(bold/styled) → optional trailing content.

Proposed: `fn build_bullet_header(bullet, label, args, style)
-> Vec<Span>`. Collapses ~25 LOC.

### F-TUI-2: overlay frame extraction

Three overlays render the same frame:
- `overlays/model_picker.rs:113-124`
- `overlays/providers.rs:229-238`
- `overlays/onboarding.rs:317-326`

Each does `centered_rect + Block::default() + Borders::ALL +
BorderType::Rounded + cyan-bold title + DIM border + Clear`.

Proposed: `fn render_overlay_frame(area, title, body_fn)
-> Rect` returning the inner rect. ~90 LOC across 3 files
becomes ~30 LOC + the helper.

### F-TUI-3: spinner unification

Two spinner systems coexist after `tui_polish_2026-04-26/PR5`:
- Original `Spinner` braille cycle (used in tool block
  headers, thinking sections, overlay loading states)
- `breathing_bullet` (used only in `render_spinner_row`)

Decide: retire braille and switch all sites to breathing, or
keep both with documented purpose split. Risk: changing
visual style of in-transcript spinners affects established
rhythm.

## PR shape

One PR landing F-TUI-1 + F-TUI-2; F-TUI-3 separate (subjective).

## Why deferred

Each touches rendering output. Worth letting the
`tui_polish_2026-04-26` round settle in real use before
piling more visual changes on. The user said they'd validate
in the morning; this round can land after that.
