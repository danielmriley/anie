# 01 — Streaming markdown: drop tail-as-plain, render full each frame

## Rationale

Finding F-1. Today
`crates/anie-tui/src/output.rs:80-148` (`StreamingAssistantRender`)
splits accumulated streaming text at the last `\n\n` boundary
outside a code fence. The committed prefix renders as full
markdown (cached); the tail renders as plain wrapped text.

Effect: until a paragraph ends, partial markdown shows as
literal characters (`**bold**`, `# heading`, `- item`). When
the paragraph closer arrives, the tail commits and snaps to
styled. Users see this as "the agent prints the raw markdown,
then converts when a line is done."

Pi and codex both render the entire accumulated message as
markdown on every update — pi via per-event re-renders
(`packages/coding-agent/.../interactive-mode.ts:2727`), codex
via commit-tick re-render
(`codex-rs/tui/src/streaming/commit_tick.rs`). Neither has
anie's tail-as-plain visual transition.

The original perf reason for this design (full markdown re-
parse per frame was expensive) is largely obsolete after PR 06
of `docs/tui_perf_2026-04-25/`. Re-parsing markdown for a
streaming buffer up to ~10 KB is sub-millisecond; even at
50 KB it's well under one frame.

## Design

Replace `StreamingAssistantRender`'s commit-boundary logic
with a single rendering pass that markdown-renders the whole
accumulated text every time. Cache the result keyed on
`(text_len, width, theme, markdown_enabled)` so that idle
frames between deltas don't re-parse.

### Sketch

```rust
struct StreamingAssistantRender {
    text: String,
    cache: Option<RenderedCache>,
}

struct RenderedCache {
    text_len: usize,    // text.len() at the time of caching
    width: u16,
    theme: MarkdownTheme,
    markdown_enabled: bool,
    lines: Vec<Line<'static>>,
    links: Vec<Vec<LinkRange>>,
}

impl StreamingAssistantRender {
    fn append_delta(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.cache = None; // text changed; cache stale
    }

    fn render(&mut self, width: u16, ctx: &RenderContext)
        -> (Vec<Arc<Line<'static>>>, Vec<Vec<LinkRange>>)
    {
        if let Some(c) = &self.cache {
            if c.text_len == self.text.len()
                && c.width == width
                && c.theme == ctx.theme
                && c.markdown_enabled == ctx.markdown_enabled
            {
                return (
                    c.lines.iter().cloned().map(Arc::new).collect(),
                    c.links.clone(),
                );
            }
        }
        let lines = if ctx.markdown_enabled {
            crate::markdown::render_markdown(&self.text, width, &ctx.theme)
        } else {
            wrap_text(&self.text, width, Style::default())
        };
        let links = crate::markdown::find_link_ranges(&lines, &ctx.theme);
        self.cache = Some(RenderedCache {
            text_len: self.text.len(),
            width, theme: ctx.theme,
            markdown_enabled: ctx.markdown_enabled,
            lines: lines.clone(),
            links: links.clone(),
        });
        // Wrap into Arc<Line> at the call boundary; inner cache
        // stays Vec<Line> to keep cache_for_size predictable.
        (lines.into_iter().map(Arc::new).collect(), links)
    }
}
```

Notes:
- The cache is invalidated on every delta (since `text_len`
  bumps). That's intentional — re-parsing on each delta is the
  cost we accept for the smoother visual.
- `text_len` is a sufficient cache key because the only way
  `text` changes is `append_delta` (append-only). If we ever
  add edit-mid-stream, switch to a revision counter.
- Tail-as-plain machinery (`committed_text`, `tail_text`,
  `cached_committed_*`, the safe-boundary scanner) all becomes
  dead code. Deletion happens in PR 06's sweep so this PR's
  diff stays focused on behavior.
  Actually — defer the deletion: leave the dead fields here for
  PR 06 to remove cleanly.

### Bench gate

PR 02 of the perf round added three keystroke benches plus the
existing render benches. This PR must clear all of them
without regression. Specifically:
- `stream_into_static_600`: must stay under 2 ms (current
  baseline 774 µs after PR 06; some regression expected since
  we re-render the streaming block fully each frame).
- `keystroke_during_stream_600`: must stay under 1 ms.
- `scroll_static_600`, `keystroke_into_idle_app_600`, `keystroke_into_long_buffer`:
  no change (these don't exercise the streaming render path).

Realistic expectation:
`stream_into_static_600` regresses from 774 µs to ~1.0–1.5 ms,
because the streaming block re-parses growing text each iter.
That's still well under one 60 fps frame and matches pi/codex
behavior.

### What about > 10 KB streaming buffers?

Add a size threshold (say 50 KB) above which we fall back to
plain wrap with a one-line system message (`"…rendering paused
for very large stream"`). This is a guard for the truly
pathological case (e.g., agent dumps a 1 MB JSON), not the
common path. Defer if the threshold is contentious.

## Files to touch

- `crates/anie-tui/src/output.rs` — `StreamingAssistantRender`
  rewrite. ~80 LOC delta (mostly deletion of `committed_text`
  / `tail_text` logic offset by simpler full-text render
  path).
- `crates/anie-tui/src/output.rs` — call sites in
  `assistant_block_lines` consume the new (lines, links) tuple
  if we expose links bundling here. Otherwise keep the current
  `block_lines → find_link_ranges` flow and only return lines
  from `render`. Pick the simpler one.
- Tests in the existing `streaming_markdown_tests` and
  `cache_tests` modules.

## Phased PRs

Single PR. The `StreamingAssistantRender` rewrite is one logical
change; splitting "delete tail-as-plain" from "re-render full
markdown" would land an in-between state where neither path is
correct.

## Test plan

1. **`streaming_renders_complete_inline_styles_immediately`**
   — append "before **bold** after", render mid-stream
   (no trailing newline). Assert the rendered output has a
   bold span over "bold" (no literal `**`).
2. **`streaming_renders_complete_link_immediately`**
   — append `[text](https://example.com)`, render. Assert link
   styled (link_text + link_url spans).
3. **`streaming_partial_emphasis_falls_back_to_literal`**
   — append `**partia` (no closer). Assert `**partia` renders
   literal (pulldown-cmark drops the unclosed emphasis; that's
   fine).
4. **`streaming_cache_serves_repeat_render_at_same_text_len`**
   — append text, render, render again without delta. Assert
   the second render comes from cache (instrument with a
   debug counter under `#[cfg(test)]`).
5. **`streaming_cache_invalidates_on_delta`** — append, render,
   append again, render. Assert cache miss on the second
   render.
6. **`streaming_cache_invalidates_on_theme_change`** — render
   under one theme, change theme, render. Assert cache miss.
   (Already exists from PR 04 — adapt to new shape.)
7. **`finalized_streaming_markdown_matches_direct_finalized_render`**
   — existing test in `cache_tests`. Must continue to pass; if
   the streaming render path now mirrors the finalized render
   path, this test gets stronger.
8. Bench gate: `cargo bench -p anie-tui --bench tui_render`
   shows `stream_into_static_600` under 2 ms,
   `keystroke_during_stream_600` under 1 ms.

## Risks

- **Per-delta re-parse cost.** The whole point of the old
  design. Mitigated by sub-millisecond pulldown-cmark cost up
  to ~10 KB. Big-stream guard via the size threshold (deferred
  unless someone hits it).
- **pulldown-cmark's handling of partial markdown.** Different
  partial inputs (`**fo` mid-emphasis, `[text` mid-link, ` ```r `
  mid-fence) need to render legibly. The library treats
  unclosed tokens as literals, which is the desired behavior.
  Worth a fixture test set with several partial states.
- **The `find_link_ranges` regression.** Currently this runs
  once per cache fill and the cache is invalidated only on
  commit. If we re-render every delta, link extraction also
  runs per delta. That's fine for streaming-block sizes, but
  worth noting in the bench commentary.

## Exit criteria

- All 8 tests above pass.
- Bench numbers within the targets.
- Manual smoke: stream a long answer with mid-paragraph bold,
  inline code, and links. Verify all three style at the moment
  the closer arrives, no per-paragraph snap.
- `cargo test --workspace` green; clippy clean.
- `00_report.md` updated with the actual bench numbers.

## Deferred

- Multi-width caching for `StreamingAssistantRender` (the
  PR 07 multi-width LineCache pattern). Streaming blocks rarely
  exist long enough at two widths simultaneously to matter;
  reopen if profile data says otherwise.
- The 50 KB pathological-stream guard. Add when someone hits it.
