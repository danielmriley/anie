# PR 2 — Per-block line cache in OutputPane

**Goal:** Stop re-wrapping every transcript block on every frame.
Each block caches its rendered `Vec<Line>` keyed by terminal
width; mutations invalidate. A 200-message transcript with one
streaming assistant at the end re-wraps exactly one block per
frame, not 200.

This is the structural fix. PR 1's frame cap bounds *how often*
we pay the cost; PR 2 reduces *the cost itself*.

## Current behavior

```rust
// crates/anie-tui/src/output.rs
fn to_lines(&self, width: u16, spinner_frame: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in &self.blocks {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(block_lines(block, width, spinner_frame));
    }
    // ...
}
```

`block_lines` invokes `wrap_text`, `wrap_spans`,
`assistant_block_lines`, `boxed_lines`, etc. on every call. These
are pure functions on the block's current state and the width.
Identical inputs produce identical outputs. We just never memo.

Pi's answer (`pi/packages/tui/src/components/markdown.ts`):

```ts
private cachedText?: string;
private cachedWidth?: number;
private cachedLines?: string[];

render(width: number): string[] {
    if (this.cachedLines && this.cachedText === this.text && this.cachedWidth === width) {
        return this.cachedLines;
    }
    // ...recompute...
    this.cachedText = this.text;
    this.cachedWidth = width;
    this.cachedLines = result;
    return result;
}

invalidate(): void {
    this.cachedText = undefined;
    this.cachedWidth = undefined;
    this.cachedLines = undefined;
}
```

We adopt the same shape, with Rust-idiomatic ownership.

## Design

### Cache storage: on the block itself

```rust
// In output.rs

#[derive(Debug, Clone)]
struct LineCache {
    width: u16,
    lines: Vec<Line<'static>>,
}

pub enum RenderedBlock {
    UserMessage {
        text: String,
        timestamp: u64,
        // new:
        cache: Option<LineCache>,
    },
    AssistantMessage {
        text: String,
        thinking: String,
        is_streaming: bool,
        timestamp: u64,
        error_message: Option<String>,
        // new:
        cache: Option<LineCache>,
    },
    ToolCall {
        call_id: String,
        tool_name: String,
        args_display: String,
        result: Option<ToolCallResult>,
        is_executing: bool,
        // new:
        cache: Option<LineCache>,
    },
    SystemMessage {
        text: String,
        // new:
        cache: Option<LineCache>,
    },
}
```

The cache lives next to the content it describes. No separate
map, no indirect indexing, no RefCell (since we already render
through `&mut self` on the OutputPane).

### Signature change: `&self` → `&mut self`

`to_lines` and `block_lines` become `&mut self` / `&mut
RenderedBlock`. `OutputPane::render` already holds `&mut self`
so this ripples cleanly:

```rust
fn to_lines(&mut self, width: u16, spinner_frame: &str) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for block in &mut self.blocks {
        if !out.is_empty() {
            out.push(Line::default());
        }
        out.extend(block.lines(width, spinner_frame).iter().cloned());
    }
    // ...
}
```

### Per-block `lines()` method

```rust
impl RenderedBlock {
    fn lines(&mut self, width: u16, spinner_frame: &str) -> &[Line<'static>] {
        if self.cache_hits(width) && !self.has_animated_content() {
            return self.cached_lines().expect("cache_hits checked it");
        }

        let computed = compute_lines_for(self, width, spinner_frame);

        if !self.has_animated_content() {
            self.set_cache(LineCache { width, lines: computed });
            return self.cached_lines().expect("just set it");
        }

        // Animated content (spinner): don't cache. Return the
        // owned Vec by stashing into a scratch slot, or just
        // return via a different path that doesn't promise a
        // reference — see "Animated blocks" below.
        self.set_cache_uncacheable(computed);
        self.cached_lines().expect("uncacheable slot")
    }
}
```

### Animated blocks (spinner)

Three sites emit content that changes every frame independent
of the block's state:

1. `AssistantMessage { is_streaming: true }` when both `text`
   and `thinking` are empty (shows `{spinner} thinking...` or
   loading state).
2. `ToolCall { is_executing: true }` with no result (shows
   `{spinner} executing...`).
3. `assistant_thinking_lines` during streaming draws an inline
   `{spinner} thinking...` suffix under the thinking block.

For these blocks, the lines depend on `spinner_frame` which
changes every tick (~10 Hz). Options:

**Option A — don't cache.** These blocks always recompute. There
are usually only 1-2 of them at once (the currently-streaming
assistant and the currently-executing tool). Acceptable.

**Option B — cache the non-spinner portion + splice the spinner
line.** Cache everything except the spinner frame. Only the
spinner line is recomputed each frame.

Recommendation: **Option A.** Simpler, correct, and the per-
frame cost of one or two live blocks is negligible compared to
the 200-block transcript walk we're eliminating. If profiling
later shows the live-block cost matters, revisit with Option B.

To implement A cleanly: `RenderedBlock::has_animated_content()`
returns true when the block's current state would produce
spinner-dependent output. That block skips the cache write.

### Invalidation

Every mutation that changes the block's content calls
`invalidate()`:

- `append_to_last_assistant` → invalidate the last block.
- `append_thinking_to_last_assistant` → invalidate the last
  block.
- `finalize_last_assistant` → invalidate the last block (also
  clears `is_streaming`, so the next render caches it for good).
- `update_tool_result`, `finalize_tool_result` → invalidate the
  matching tool-call block.
- `add_*` (add_block, add_user_message, etc.) → new blocks
  start with `cache: None`.

Width changes invalidate implicitly via the `cache.width !=
width` check.

### Ratatui alignment

Ratatui's `Terminal::draw()` already double-buffers via `Buffer`
and writes only changed cells to the terminal. We're not
replacing that — we're reducing the cost of *producing* the
`Vec<Line>` that gets fed into `Paragraph::new(...)` each
frame. The `Paragraph` still gets the same input as before; it
just gets it from a cache on cache hits.

No new ratatui idiom needed. No `StatefulWidget`, no custom
widget, no backend changes.

## Files

- `crates/anie-tui/src/output.rs` — only file touched.

## Test plan

| # | Test |
|---|------|
| 1 | `block_lines_cache_hit_skips_recomputation` — render once, mutate a counter inside the compute path (test hook), render again at the same width; counter did not increment. |
| 2 | `block_lines_cache_invalidated_by_append` — render, append text to the block, render; counter incremented exactly once per call. |
| 3 | `block_lines_cache_invalidated_by_width_change` — render at width W1, render at W2; cache missed. |
| 4 | `animated_blocks_do_not_cache` — render a streaming assistant with empty text + thinking on tick N and tick N+1; both invocations recomputed (not a bug, and reflects the spinner changing). |
| 5 | `cached_blocks_produce_identical_lines_to_uncached` — for every existing `block_lines` output, cached invocation == uncached. This is a golden comparison against the pre-refactor behavior; belongs as a loop test across several fixtures. |
| 6 | `finalize_clears_streaming_flag_and_caches_next_render` — finalize a streaming block, render twice; second render is a cache hit. |
| 7 | Existing render-snapshot tests (`tests::assistant_block_*`, etc.) still pass. |

## Exit criteria

- [ ] `RenderedBlock` carries a `LineCache` field per variant.
- [ ] `to_lines` / `block_lines` take `&mut self`.
- [ ] Every mutation site invalidates.
- [ ] Animated (spinner-bearing) blocks skip the cache write.
- [ ] Tests 1-7 pass.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] Manual: 200-block transcript with streaming assistant feels
      snappy; no input lag.

## Risks

- **Forgotten invalidation.** Adding a mutation method in the
  future and forgetting to invalidate would cause stale
  rendering. Mitigations: keep mutation methods few and
  co-located; test 5 (golden comparison) catches at least one
  class of regression.
- **`Line<'static>` cloning cost.** The cache hands back a
  reference, but if callers clone (e.g., to extend a Vec), each
  frame still pays N clones for N lines. This is strictly less
  expensive than re-wrapping, and it's what the current code
  already does implicitly. Monitor post-merge; if needed, switch
  the renderer to stream lines through the Paragraph via `Cow`.
