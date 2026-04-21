# Plan 05 — markdown rendering in the TUI

**Tier 3 — structural but well-scoped, biggest UX win.**

Bring markdown-aware rendering to assistant output. Headings,
lists, code blocks with syntax highlighting, tables, blockquotes,
emphasis, and optional OSC 8 hyperlinks. pi's markdown widget at
`packages/tui/src/components/markdown.ts` is the model —
specifically its structure (Markdown AST → styled Line sequence
with per-component caching), not its TypeScript source.

## Rationale

Today, anie renders assistant output as plain-wrapped text with
ratatui spans. Headings are inline text prefixed with `#`. Code
blocks are unindented prose. Tables are literally pipe-and-dash
ASCII. Inline code is indistinguishable from surrounding text.
Bullet lists are plain `-` chars in flow.

For a coding agent whose output is heavily code-and-snippet, this
is the single largest readability gap vs pi. Every other piece of
the harness can be fast and correct — users spend their time
reading assistant output, and it looks bad.

## Design

### Dependency choice

Use the `pulldown-cmark` crate (a CommonMark-compliant streaming
parser) + `syntect` (for code syntax highlighting). Both are
well-maintained and widely used in the Rust ecosystem. This is
*not* a full rewrite — we're using established libraries.

Why not termimad? termimad renders to its own term backend, not
ratatui's `Line`/`Span` model. Integration would be awkward.

### Architecture

A new module, `crates/anie-tui/src/markdown/`:

```
markdown/
  mod.rs           — public API: render_markdown(text, width) -> Vec<Line<'static>>
  parser.rs        — pulldown_cmark wrapper, produces an event stream
  layout.rs        — transforms events into a Vec<Line>, handles wrapping
  theme.rs         — Style definitions per element (heading, code, link, etc.)
  syntax.rs        — code-block highlighting via syntect
  link.rs          — OSC 8 hyperlink emission (gated by TerminalCapabilities)
```

Public entry:

```rust
pub fn render_markdown(
    text: &str,
    width: u16,
    capabilities: &TerminalCapabilities,
    theme: &MarkdownTheme,
) -> Vec<Line<'static>>;
```

### Rendering responsibilities

| Element | Treatment |
|---------|-----------|
| ATX heading `#` / `##` / `###` | Bold + color-coded; h1 = Cyan bold, h2 = Magenta, h3 = Yellow. Blank line after. |
| Paragraph | Plain text, wrapped. |
| Bold (`**`) | `Modifier::BOLD`. |
| Italic (`*`) | `Modifier::ITALIC`. |
| Strikethrough (`~~`) | `Modifier::CROSSED_OUT`. |
| Inline code (`` ` ``) | `fg: Yellow, bg: reset`, surrounded by ` markers. |
| Code block ` ``` lang ` | Bordered box like current ToolCall; language-aware syntax highlight via syntect. |
| Bullet list `-` | `  • ` prefix; nested lists indent by 2 spaces per level. |
| Ordered list `1.` | `  1. ` prefix, numbering preserved. |
| Blockquote `>` | `│ ` gutter in DarkGray, body in italic. |
| Table | Rendered with unicode box-drawing, column-width auto-sizing with a max-width clamp. |
| Link `[text](url)` | When `capabilities.supports_osc8_hyperlinks`: OSC 8 wrapped `text`. Otherwise `text (url)` or just `text` with underline. |
| Horizontal rule `---` | Full-width `─` line in DarkGray. |
| HTML / raw HTML | Render as plain text (don't execute, don't format). |
| Soft break | Newline → space (CommonMark default). |
| Hard break (`<br>` / two-space) | Newline → actual `\n`. |
| Raw ANSI in code blocks | Preserve or strip? Pi preserves — if syntect emits ANSI we need to handle both correctly. Default: strip syntect's ANSI and re-emit via ratatui spans. |

### Caching and streaming

Two categories of block matter here:

1. **Finalized assistant blocks** — the cache from PR 2 of
   `tui_responsiveness` kicks in: `(content, width) → Vec<Line>`
   memoized per block. Markdown rendering happens once on
   `finalize_last_assistant`, cached, then re-used until width
   changes.
2. **Streaming assistant blocks** — these are deliberately
   *excluded* from the cache (`block_has_animated_content`
   returns true for `is_streaming`). Rendering a streaming
   block re-parses the content every frame at up to 30 fps.

Parsing markdown every frame during streaming is expensive —
the whole reason PR 2 existed was to stop doing O(transcript)
work per frame. So: **streaming blocks render as plain wrapped
text, not markdown. Finalized blocks render as markdown.** The
transition happens naturally in `finalize_last_assistant`
because that's also the cache-invalidation point.

UX implication: during streaming the user sees raw markdown
tokens (`**bold**` literally) until the block finalizes, at
which point it "settles" into rendered markdown. Pi has the
same behavior — its markdown component is a finalized-block
concern.

### Integration with existing output

Two modes for assistant text:

1. **Markdown mode (default, finalized blocks only).**
   `render_markdown` produces the line vector inside
   `assistant_answer_lines` when `is_streaming == false`.
2. **Plain mode (fallback and all streaming blocks).** Current
   `wrap_text` behavior.

Config toggle: `ui.markdown_enabled: bool`. The existing
`UiConfig` struct in `crates/anie-config/src/lib.rs` already
uses this pattern (see `slash_command_popup_enabled`), so
extend rather than create a new `rendering` section. Slash-
command `/markdown on`/`off` flips the flag at runtime.

Tool-result rendering (inside the boxed tool-call display) stays
plain — bash output isn't markdown and shouldn't be parsed.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/Cargo.toml` | Add `pulldown-cmark` + `syntect`. |
| `crates/anie-tui/src/markdown/mod.rs` | New. |
| `crates/anie-tui/src/markdown/parser.rs` | New. |
| `crates/anie-tui/src/markdown/layout.rs` | New. |
| `crates/anie-tui/src/markdown/theme.rs` | New. |
| `crates/anie-tui/src/markdown/syntax.rs` | New. |
| `crates/anie-tui/src/markdown/link.rs` | New. |
| `crates/anie-tui/src/output.rs` | `assistant_answer_lines` calls into markdown renderer. |
| `crates/anie-tui/src/lib.rs` | Re-export `MarkdownTheme` if useful to tests. |
| `crates/anie-config/src/lib.rs` | Extend the existing `UiConfig` with `markdown_enabled: bool` (pattern-match `slash_command_popup_enabled`). |
| `crates/anie-tui/src/commands.rs` | `/markdown on`/`off` toggle. |

## Phased PRs

### PR A — scaffolding + parser

1. Cargo deps added.
2. `parser.rs` wraps `pulldown-cmark::Parser` and yields events.
3. `layout.rs` implements a skeletal rendering that handles
   paragraphs, headings, bold/italic, and one-line code spans.
   Everything else falls back to plain text.
4. `theme.rs` defines `MarkdownTheme` with default styles.
5. `render_markdown` wired in; gated behind a `#[cfg(test)]` flag
   so we don't swap production behavior yet.
6. Snapshot-style tests (via `insta` or string comparisons)
   against fixtures in `crates/anie-tui/fixtures/markdown/`.

### PR B — code blocks + syntax highlighting

1. `syntax.rs` wraps `syntect` with a lazily-loaded syntax set
   (ship with default Rust / JS / TypeScript / Python / shell
   grammars to keep binary size reasonable).
2. Code blocks get a border (same box style as tool calls with a
   language label in the title row).
3. Theme integration — syntect's default themes like
   `InspiredGitHub` work well in light terminals; ship both a
   light and dark theme, select based on env (default dark).
4. Tests: render a multi-line code block, assert the box
   structure + language label.

### PR C — lists, tables, blockquotes, horizontal rules

1. List handling: track nesting depth, emit `  •`, `    ◦`,
   `      ▪` prefixes.
2. Ordered-list numbering: preserve the source number (the
   parser gives us the raw position).
3. Blockquotes: `│` gutter in DarkGray.
4. Tables: walk the cells, compute column widths, emit unicode
   box-drawing.
5. Horizontal rules: full-width `─` line.
6. Tests per element.

### PR D — links + OSC 8 integration

1. `link.rs` emits OSC 8 when `capabilities.supports_osc8_hyperlinks`.
2. Fallback format: `[text] (url)` in DarkGray with the text
   underlined.
3. Tests: assert the escape sequence emission when enabled, plain
   text when disabled.

### PR E — ship

1. `UiConfig::markdown_enabled = true` by default; serde defaults
   preserve forward compat.
2. `assistant_answer_lines` in `output.rs` branches on
   `is_streaming`: plain wrap when streaming, markdown when
   finalized. Invocation threaded through a
   `RenderContext { capabilities, markdown_enabled, theme }`
   passed in alongside width.
3. Add `/markdown on`/`off` slash command that flips the
   runtime flag.
4. Document the streaming-vs-finalized rendering difference in
   the output module's top-of-file comment so future maintainers
   don't "optimize" by moving markdown into the streaming path.
5. Manual smoke: generate a response with headings, code blocks,
   lists, links, and a table. Verify raw markdown during
   streaming, rendered markdown on finalize.

## Test plan

Per-PR tests above, plus cross-cutting:

| # | Test | Where |
|---|------|-------|
| 1 | Plain text renders unchanged (no markdown tokens → plain output). | `markdown/layout.rs` |
| 2 | Width changes re-render correctly. | `markdown/layout.rs` |
| 3 | Code-block language fallback: unknown language renders without highlight, with border. | `markdown/syntax.rs` |
| 4 | Nested list numbering resets correctly. | `markdown/layout.rs` |
| 5 | OSC 8 disabled under tmux even if terminal supports it. | `markdown/link.rs` |
| 6 | Snapshot suite against ~10 curated markdown fixtures. | `tests/markdown_render.rs` |
| 7 | OutputPane cache still works (markdown rendering is a pure function of `(text, width, capabilities)`). | existing cache tests |

## Risks

- **Binary size.** syntect's default syntax set is ~2 MB. We can
  ship a curated subset (Rust + TS/JS + Python + shell = ~500
  KB) to keep the release binary reasonable. Measure post-PR.
- **syntect's theme compatibility.** syntect expects TextMate
  themes; we may need to convert. Ship with two bundled themes
  (one dark, one light), both TextMate-format.
- **pulldown-cmark's CommonMark strictness.** Some LLMs emit
  non-spec markdown (e.g., tables without leading `|`,
  underscores in code ident without escaping). Compare output
  across a set of real model responses early — the parser
  settings (`Options::ENABLE_TABLES`, etc.) are configurable.
- **Markdown rendering is slow.** Per-paint cost is the biggest
  risk. Mitigation: OutputPane's per-block cache already
  memoizes the rendered `Vec<Line>`. A single block re-parses
  only on content mutation, not every frame.
- **Performance regression on the hot path.** Markdown parsing +
  syntect run inside the streaming assistant block (which is
  uncached — it's animated). Measure under a long streamed
  response. If too slow, fall back to rendering only the
  committed (non-streaming) assistant blocks as markdown; keep
  the streaming block plain-text until finalize.

## Exit criteria

- [ ] All five PRs merged.
- [ ] `rendering.markdown_enabled` default true; `/markdown off`
      toggles cleanly.
- [ ] Snapshot suite covers headings, lists, code blocks
      (Rust, TS, Python, shell), tables, blockquotes,
      horizontal rules, links (OSC 8 and fallback).
- [ ] No observable TUI latency regression on a long agent run
      vs the plain-text baseline.
- [ ] Manual visual smoke against 3+ real assistant responses.

## Deferred

- **Math rendering** (LaTeX, `$...$`). Most coding tasks don't
  need it; ship when a math-heavy use case lands.
- **Image embeds** (`![alt](url)`). Ties to inline image
  rendering (Plan 04 extension). Render as `[image: alt]`
  placeholder for now.
- **Per-user theme customization.** Ship with two built-in
  themes (light, dark, auto-selected). Config-driven custom
  themes can come later.
