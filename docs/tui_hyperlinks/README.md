# Plan 10 — clickable hyperlinks in the TUI

**Feature. Makes URLs in agent output clickable.**

Plan 05 PR D deferred OSC 8 escape emission because ratatui's
`unicode-width::UnicodeWidthStr` counts the printable chars
inside an OSC 8 escape as visible cells, breaking line-width
calculations. The fallback shipped: show `[text] (url)` with
the text underlined.

User wants the real hyperlinks.

## Approach pivot (2026-04-22)

After surveying ratatui 0.29's Buffer / Cell API, OSC 8 via a
wrapping backend remains substantial work (~200+ LOC) and
relies on running custom code between ratatui's buffer flush
and crossterm's stdout. The abstractions aren't designed for
this extension point; any implementation is brittle against
ratatui releases.

**Simpler path that works on every terminal with mouse
capture (which anie already enables):** click-to-open.

- Record `(line_index, col_start, col_end, url)` ranges during
  markdown rendering — specifically for the visible `(url)`
  fallback text in the link's link_url-styled span.
- On `MouseEventKind::Down(Left)` in the output pane, translate
  screen coords → line index (via scroll_offset + pane_y) →
  look up hit registry → if url matched, `opener::open(url)`.
- Only the URL text itself is clickable, not the hyperlink
  text. Precise target; no false positives from clicking prose
  that happens to contain a link.

Loses vs. OSC 8:
- Native hover-to-preview (browsers / terminals show the URL
  above the cursor on hover for OSC 8 links). Click-to-open
  has no hover indication.
- Link stays clickable even when the user scrolls — hit test
  accounts for scroll_offset.

Wins vs. OSC 8:
- Works on any terminal anie supports (mouse capture was
  already required for scroll).
- No terminal-capability gating needed — if mouse works, clicks
  work.
- Implementation is ~50 LOC + markdown rendering hook.

## Rationale

OSC 8 is the BEL-terminated escape sequence modern terminals
(iTerm2, WezTerm, Kitty, recent gnome-terminal, etc.) render
as clickable links:

    \x1b]8;;https://example.com\x07visible text\x1b]8;;\x07

Inside ratatui, a `Span` carries only `content: Cow<'static,
str>` + `style: Style`. The render pipeline asks each span for
its display width via `UnicodeWidthStr::width(content)`, which
sums printable chars. For the escape above, the `;;url\x07`
part counts as extra cells. On a 120-col terminal with a 60-char
URL, ratatui thinks the line is 180 cells wide, and layout
wrapping / cursor positioning / scroll math all break.

## Design

Three approaches surveyed:

### Option A: literal-span marker + backend interposition

Store links as two sibling spans the widget layer treats as
normal, and have a backend shim rewrite them into OSC 8 at
flush time:

    [Span{content: "<ZWSP>📎0<ZWSP>", hidden_link: "https://..."},
     Span{content: "visible text", style: underline},
     Span{content: "<ZWSP>📎/<ZWSP>", hidden_link: ""}]

- Zero-width markers in content make ratatui's width calculation
  correct (ZWSP has width 0).
- A custom `Backend` wrapper intercepts `Buffer` flushes, scans
  for the markers, rewrites to OSC 8 escapes before emitting.

Complex but principled. Requires a custom backend wrapping the
existing `CrosstermBackend`.

### Option B: emit OSC 8 at paint time only on fully-terminal lines

When the layout engine emits a line and we KNOW it won't be
re-measured (final rendering step), append OSC 8 directly. For
the block-cached `Vec<Line>`, the cache stores pre-OSC-8 data;
at render time we transform.

Still needs a seam in the backend — ratatui doesn't expose a
"just emit these styled bytes" hook on top of the buffer.

### Option C: gate on terminal-capability + use a wrapping backend

Detect OSC 8 support once at startup (we already have
`TerminalCapabilities::hyperlinks` from Plan 04). If available,
wrap `CrosstermBackend` with our own backend that:

1. Receives `Buffer` contents as ratatui normally renders.
2. Before calling `queue!` on crossterm, scans each cell for a
   "link sentinel" (sentinel chars + followed by URL encoded in
   unused style attributes via some escape mechanism).
3. Emits OSC 8 opening + cell contents + closing.

This generalizes Option A. Still a fair bit of code.

### Recommendation: ship Option A in stages

**Stage 1 — research prototype.** Build the smallest possible
demo: a single hard-coded OSC 8 link in a markdown-rendered
line, using a bespoke wrapping backend, end-to-end in a test
TUI. Prove the concept before committing.

**Stage 2 — production wiring.** If Stage 1 works, integrate:
- New `LinkRegistry` in `anie-tui` mapping sentinel-ID → URL.
- Markdown layout adds entries to the registry when it sees
  `Tag::Link(_)` and emits sentinel-bracketed spans.
- Backend wrapper scans each rendered line for the sentinels
  and emits OSC 8 around them.

**Stage 3 — fallback unchanged.** If `capabilities.hyperlinks`
is false (e.g., in tmux without passthrough, on older
terminals), the registry emits the existing `(url)` fallback.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/src/terminal/backend.rs` (new) | Wrapping backend over `CrosstermBackend`. |
| `crates/anie-tui/src/terminal.rs` | Replace `CrosstermBackend` with wrapping backend when `capabilities.hyperlinks`. |
| `crates/anie-tui/src/markdown/link.rs` | Emit sentinel spans when hyperlinks enabled; fallback otherwise. |
| `crates/anie-tui/src/markdown/link_registry.rs` (new) | Sentinel-ID → URL mapping per render. |
| `crates/anie-tui/src/markdown/layout.rs` | `LineBuilder` tracks the registry while rendering. |

## Phased PRs

### PR A — research prototype

1. Zero-width sentinel constants.
2. Wrapping backend that scans and emits OSC 8.
3. Demo test: single hard-coded hyperlink renders correctly.

### PR B — registry + markdown integration

1. `LinkRegistry` + `LinkId` type.
2. `LineBuilder` populates the registry on `Tag::Link`.
3. Backend wrapper uses the registry to resolve sentinels.

### PR C — capability gating + fallback

1. Read `TerminalCapabilities::hyperlinks` at TUI startup.
2. Disable OSC 8 path under tmux + older terminals.
3. Existing `(url)` fallback stays for the disabled case.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | Sentinel-wrapped span has `UnicodeWidthStr::width() == visible_text_width`. | unit |
| 2 | Backend wrapper emits OSC 8 bytes for a link span. | unit, read from a `Vec<u8>` sink backend. |
| 3 | Capability-disabled path emits `(url)` fallback. | unit. |
| 4 | Scroll math on a block containing a hyperlink is correct (cursor position matches visible text). | integration, manual. |

## Risks

- **Broken scroll / cursor math.** This is the exact bug the
  plan started with, just moved to a different layer. The
  sentinels must have zero width AND the backend wrapper must
  not double-count them.
- **Backend abstraction drift.** ratatui may ship hyperlink
  support upstream before we finish. Check before Phase 2.
- **Slow backend scan.** Scanning every cell for sentinels
  costs on every redraw. The `(url)` fallback costs nothing.
  Measure after PR B.

## Exit criteria

- [ ] PRs A + B merged.
- [ ] A hyperlink in agent output is clickable in iTerm2 (or
      equivalent) on the user's machine.
- [ ] Layout (scroll, wrap, cursor) unaffected vs. the
      non-link case.
- [ ] Fallback path still renders `(url)` correctly under
      tmux / non-OSC-8 terminals.

## Deferred

- **Auto-link detection in plain assistant text.** Today only
  markdown links are candidates. Auto-detecting raw URLs in
  non-markdown-mode is a separate polish item.
- **OSC 8 id attribute for stable click identity.** Only needed
  if we grow UI hooks that track which link was clicked.
