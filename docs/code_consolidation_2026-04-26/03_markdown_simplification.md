# 03 — Markdown layout simplification

Adopt pi's simpler shape for markdown rendering. Targets the
1,866 LOC `crates/anie-tui/src/markdown/layout.rs` (vs. pi's
852 LOC for the same feature set).

## Items

### F-MD-4: relax over-specific tests (do first)

`table_renders_with_unicode_box_drawing` and ~9 others pin
exact glyph characters and padding widths. Before any
layout refactor, relax these to behavioral assertions:
- Content present
- Columns aligned
- Headers separated from body

Estimated effort: 1-2 tests need loosening; consolidates 10
tests into 2-3 behavioral.

### F-MD-1: table layout simplification

Adopt pi's single-pass token walk for table rendering instead
of anie's multi-pass column negotiation:

Anie today (`layout.rs:662-980`):
- `compute_column_widths` (66 LOC — proportional shrink)
- `wrap_table_row` (18 LOC — transpose-then-wrap)
- `wrap_plain_text_cell` (50+ LOC — custom word-break)
- `pad_cell` (23 LOC)
- `table_data_row` + `table_border_line` (~40 LOC)

Pi (`packages/tui/src/components/markdown.ts:679-850`):
- Single `renderTable()` walking tokens, emitting lines
  inline. Single fallback check, no proportional shrink loop.

Adopting saves 80-100 LOC.

### F-MD-2: list state machine

Drop `pending_first_line_prefix` (one-shot side-channel
state). pulldown-cmark's event nesting already gives depth.
Inline bullet-marker construction at `Start(Item)`. ~40-50
LOC.

## PR shape

One PR: F-MD-4 first (test relaxation), then F-MD-1 + F-MD-2
together (related refactors). Risk is medium because
cell-wrapping fidelity could regress for edge-case markdown.

## Why deferred

The user just had a markdown-rendering polish round
(`tui_polish_2026-04-26/PR1`, `PR2`, `PR4`). Real-use
validation matters before adding more changes to this layer.
