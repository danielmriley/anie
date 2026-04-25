# 04 — Streaming hot path

## Rationale

Findings F-8, F-9, F-10, F-15. The user's "output feels a little
sluggish" complaint surfaces during streaming, when the per-block
cache is bypassed (animated blocks always miss) and every paint
re-runs the full block-render pipeline.

Three concrete costs run on every streaming-block render:

- **F-8**: `find_link_ranges` walks the entire streaming block
  every frame. Estimated 2–5 ms/frame for a 100-line streaming
  answer. The function (`crates/anie-tui/src/markdown/mod.rs:57-102`)
  also calls `chars().count()` twice on the same span content
  (`mod.rs:68`, `:79`).
- **F-9 / F-15**: `format_tool_header_spans` and
  `boxed_lines` / `prefix_lines` allocate fresh strings per
  render — `prefix.to_string()`, `format!("…")`, `" ".repeat()`,
  `"─".repeat()`. On cache miss they're unavoidable; on
  streaming/executing blocks they fire every frame.
- **F-10**: `StreamingAssistantRender` cache key is
  `(width, markdown_enabled)` but doesn't include theme. Stale
  the moment theme switching ships.

This PR targets all four. They share the same root: streaming
blocks are special-cased everywhere except where they benefit
from caching.

## Design

### F-8: throttle link extraction on streaming blocks

Streaming blocks change on every delta — but the *committed
prefix* of the markdown changes only at commit boundaries (a
blank line outside a fence; see
`crates/anie-tui/src/output.rs:80-140`). Link ranges in the
committed prefix don't change between commits.

**Plan**: cache the link map for the committed prefix on
`StreamingAssistantRender`, recompute only when the cache
invalidates (same trigger as `cached_committed_lines`). For the
tail portion, run `find_link_ranges_in_line` only on the
plain-wrapped tail lines — usually a handful, not the whole
block.

This drops `find_link_ranges` cost on a streaming block from
O(total_lines × spans) per frame to O(tail_lines × spans) per
frame plus a one-shot O(committed_lines × spans) per commit
boundary.

### F-8b: dedupe `chars().count()` calls in `find_link_ranges_in_line`

`crates/anie-tui/src/markdown/mod.rs:64-102`. Lines 68 and 79
both call `span.content.chars().count()` on the same span.
Compute once, reuse. Trivial.

### F-9 / F-15: pre-compute static parts of bullet/box headers

The bullet/box header building blocks are the bullet glyph
(`•` / `└`), the verb (`Ran`, `Reading`, etc.), the
border-fill repeats (`─...─`). The verb depends only on tool
kind and execution state; the fill widths depend on terminal
width. None of them depend on per-frame state.

**Plan**:

1. Move the verb lookup into a `const` map or a `match` that
   returns `&'static str` rather than allocating. Audit `to_string()`
   sites in `format_tool_header_spans` and replace with
   `Span::styled(&'static str, style)` / `Span::raw` /
   `Span::from(Cow::Borrowed(...))` where possible.
2. For the fill characters (`"─".repeat(n)`, `" ".repeat(n)`) in
   `boxed_lines`, keep a `OnceLock<String>` of the largest fill
   used so far and slice it instead of allocating per call.
   Or: compute once per render (not per body line) and reuse.
3. The spinner frame string (`output.rs:1414-1446`) — pass
   `&str` through, don't `.to_string()` it inside the helper.

These are micro-optimizations; the bench will tell us if the
cumulative win is worth the patch size. If F-8 alone closes the
visible gap, F-9/F-15 can be tied off as "addressed by
no-clone-on-static-strings audit."

### F-10: include theme in StreamingAssistantRender cache key

`crates/anie-tui/src/output.rs:121-139`. Add the theme — or a
hashable theme-id — to the cache validity check:

```rust
if self.cached_committed_width == Some(width)
    && self.cached_committed_markdown_enabled == ctx.markdown_enabled
    && self.cached_committed_theme_id == ctx.theme.id()
{
    return self.cached_committed_lines.clone();
}
```

`MarkdownTheme` is `#[derive(Copy)]` (small struct), so a
plain `==` would work, or add a cheap `id()` accessor that
hashes the relevant fields once. Pick whichever is more
maintainable as the theme grows.

This is forward compat for the deferred theme-switching feature.
Cheap to fix now; the moment a theme command ships, this
becomes a bug if not addressed.

## Files to touch

- `crates/anie-tui/src/markdown/mod.rs` — dedupe `chars().count()`,
  expose `find_link_ranges_in_line` if not already (it appears
  to be a private helper at `mod.rs:64`).
- `crates/anie-tui/src/output.rs` — cache link map on
  `StreamingAssistantRender`; theme key; static-string audit on
  bullet/box helpers.
- Tests in both modules.

## Phased PRs

Bundle. The streaming render path is one logical unit and these
fixes share the cache-invalidation reasoning. Splitting would
re-litigate the same invalidation predicates three times.

If reviewers prefer split:
- PR 04a: streaming link map cache (F-8 + F-8b) — biggest win.
- PR 04b: theme in streaming cache key (F-10) — forward compat.
- PR 04c: bullet/box static-string audit (F-9, F-15) — micro
  win, do last after 04a's bench impact is known.

## Test plan

1. **`streaming_link_map_does_not_recompute_within_commit_window`**
   — append several deltas without crossing a commit boundary;
   render N times; assert link-extraction calls don't grow with
   N. Instrument with a `#[cfg(test)]` counter.
2. **`streaming_link_map_invalidates_on_commit_boundary`** —
   append `\n\n` outside a fence; assert next render
   recomputes.
3. **`streaming_cache_invalidates_on_theme_change`** — render
   with one theme, switch, render; assert recompute path runs.
4. **`tool_header_static_strings_use_cow_borrowed`** —
   structural test: build a tool block, assert no Span content
   strings allocated when only static parts changed.
5. **PR 02 keystroke bench** — `keystroke_during_stream_600`
   improves. This is the most user-visible scenario.

## Risks

- **Link-map cache correctness during streaming.** The committed
  prefix is reused across renders; the tail isn't. Mixing them
  must preserve line indexing for mouse hit-tests. The link
  map is `Vec<Vec<LinkRange>>` (one inner vec per line) — keep
  that shape, just split the work.
- **Theme equality.** If the eventual theme system grows
  fields like syntect colors that aren't `Copy`, the
  cache-key comparison gets non-trivial. Mitigation: keep a
  `theme_id: u32` (incremented per theme change) on
  `RenderContext` rather than comparing the struct.
- **Static-string audit footgun.** ratatui's `Span` content is
  `Cow<'static, str>` already; the cost is the `.to_string()`
  *we* call before passing in. Fixing means changing helpers
  from taking `&str` and returning owned, to taking `&'static
  str` (or `impl Into<Cow<'static, str>>`). Easy to introduce
  a borrow-checker tangle if not careful.

## Exit criteria

- `find_link_ranges` does not run per-frame on the committed
  prefix of a streaming block.
- `StreamingAssistantRender` cache key includes theme.
- `format_tool_header_spans` does not allocate `String`s for
  static glyph/verb pieces.
- New keystroke bench `keystroke_during_stream_600` improves.
- `stream_into_static_600` from the original bench either
  improves or is unchanged (must not regress).
- `cargo test --workspace` green; clippy clean.

## Deferred

- Incremental markdown parsing of the committed prefix. The
  current "re-parse the committed prefix on each invalidation"
  is fine because invalidation only fires on commit boundaries.
- A full Span borrow audit through the markdown layer (Findings
  F-13, F-14). Bigger scope; do only if profile data points at
  it after F-8/F-9/F-10/F-15 land.
