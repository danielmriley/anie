# Plan 08 — Codex-style streaming collector

## Rationale

A user comparison to OpenAI's Codex CLI surfaced an
architecture worth porting from `codex-rs/tui/src/` —
specifically the `MarkdownStreamCollector` / `StreamState` /
`StreamController` trio. The pattern is:

1. **Newline-gated commit.** While a stream is in flight,
   buffer the incoming text in a `String`. Only *commit*
   lines once a `\n` arrives. Partial in-flight lines stay in
   the buffer and are rendered in a lightweight "tail" region
   that's cheap to redraw.
2. **Per-line FIFO queue** with arrival timestamps. Committed
   lines flow into a queue; the render loop pops them at a
   "commit tick" cadence for a type-writer effect.
3. **Adaptive drain.** If the queue grows beyond a threshold
   (provider streams faster than we can paint), drain
   multiple lines per tick to catch up.

Contrary to the original feedback's framing, Codex does
**not** do incremental markdown parsing — it re-parses the
full buffer per newline-bearing delta, then extracts only
the newly-complete line suffix. That's cheaper than it
sounds: pulldown-cmark on accumulated text is O(text_len)
but happens at most once per newline arrival, not per token.
anie currently wraps the full streaming text on *every*
delta via `wrap_text` in the streaming branch of
`block_lines` — which is per-token, not per-newline.

**Benefit vs the other plans:**

- Plan 04 (streaming coalescing) batches multiple deltas per
  frame. That reduces the number of rewrap calls per second
  from delta-rate to frame-rate.
- Plan 08 (this plan) further cuts the cost *per rewrap*
  because we rewrap only committed lines (which never change
  after commit — they can be fully cached) plus the
  incomplete tail (short, cheap). After the assistant message
  is done, the tail commits too.

Put differently: Plan 04 reduces how often we re-render the
streaming block; Plan 08 reduces how expensive each re-render
is. They compose.

## What we explicitly don't adopt from Codex

- **The `tui-markdown` crate** — Codex doesn't use it either.
  anie's custom renderer is already more complete (supports
  tables, links, syntax-highlighted code blocks). Replacing
  it would be a regression.
- **The full `ChatWidget` state machine.** Codex's is ~2000
  LOC of accumulated logic. We only want the collector shape,
  not the wider widget.
- **Timed type-writer pacing.** Codex paces committed lines
  to simulate typing. anie has no such visual; skip it. We
  commit-as-fast-as-available.

## Design

### 8.1 The collector

Replace `last_assistant.text: String` (current
`output.rs:~465`) with:

```rust
pub struct StreamingMarkdownBuffer {
    /// Committed lines — markdown syntax that has survived a
    /// final `\n`. Never changes after commit.
    committed_text: String,
    /// Rendered output for committed_text at the last
    /// (width, theme). Cached like any finalized block;
    /// invalidated on width/theme change, extended on commit.
    committed_lines: Arc<Vec<Line<'static>>>,
    /// The tail after the last newline. May be partial;
    /// re-rendered cheaply on every delta via wrap_text
    /// (plain, not full markdown), since we can't parse
    /// incomplete markdown safely.
    tail_raw: String,
    tail_lines: Vec<Line<'static>>,
}
```

### 8.2 Delta application (replaces per-delta invalidate)

```rust
fn push_delta(&mut self, delta: &str) {
    // Fast path: no newline in delta → just extend tail.
    let mut remaining = delta;
    while let Some(nl_idx) = remaining.find('\n') {
        let (line_chunk, rest) = remaining.split_at(nl_idx + 1);
        self.tail_raw.push_str(line_chunk);
        // Commit the completed line: move tail → committed.
        self.committed_text.push_str(&self.tail_raw);
        self.tail_raw.clear();
        remaining = rest;
    }
    self.tail_raw.push_str(remaining);
    // tail_lines is invalidated; committed_lines is NOT.
    self.tail_lines.clear();
}
```

On commit, we do **not** re-render the whole `committed_text`
every time. Instead:

- On the *next* render, if `committed_lines` is stale (or
  first render at this width), do one full markdown re-parse
  of `committed_text` → `Arc<Vec<Line>>`. Cache it.
- `tail_lines` is re-rendered each frame via plain wrapping
  only (`wrap_text`, not markdown) since the tail might contain
  unclosed fences etc. — short and cheap.
- `build_lines` concatenates `committed_lines.iter().cloned()`
  + `tail_lines.iter().cloned()`.

The key discipline: **we don't rewrap committed text per
delta anymore**. We rewrap it at most once per width change
(or once on finalization). Per-delta cost collapses to
"extend a String + rewrap the short tail."

### 8.3 Finalization

When `StreamDone` arrives: push a synthetic `\n` if needed
so the tail commits, do one final markdown re-render of the
now-complete `committed_text`, store as the normal finalized
block. The block is then an ordinary cached block under
Plan 03/05's caching rules.

### 8.4 The "rewrap on width change" case

When width changes during a stream, `committed_lines` is
stale. We re-parse from `committed_text` at the new width
(one-shot, not per-delta). With Plan 03's Arc-wrapping and
Plan 05's width-keyed cache, this is the same code path as
finalized-block rewrap-on-resize.

## Files to touch

- `crates/anie-tui/src/output.rs`: replace the streaming
  branch of `block_lines` with the collector. This is the
  biggest structural change in the plan set.
- `crates/anie-tui/src/markdown/mod.rs`: may need an
  `append_markdown` entry-point that returns
  `Arc<Vec<Line>>` directly (or keep the current
  `render_markdown` signature — either works).
- Tests: new module under `output.rs` for the collector's
  behavior (newline handling, tail edge cases, width change
  during stream, mixed `\r\n`).

## Phased PRs

### PR-A: collector struct + unit tests (no wiring)

Land `StreamingMarkdownBuffer` with full unit tests, unused
by the render loop. Behavior tested in isolation.

- Tests: empty → push → commit on `\n` → tail after `\n` is
  empty; unterminated trailing text lives in tail;
  multi-line delta commits all complete lines.
- Exit: type exists, tests pass, no production call sites.

### PR-B: wire into `OutputPane`

Replace `append_to_last_assistant` → `push_delta`; replace
the streaming branch of `block_lines` with
`committed_lines + tail_lines`.

- Regression: existing streaming-render tests must pass
  byte-identical output for complete transcripts. During
  streaming, visual parity is subjective; add a snapshot
  test that drives 100 deltas and checks the final rendered
  buffer.
- Exit: `stream_into_static_600` bench p50 drops ≥50% vs
  Plan 03's result.

### PR-C: finalize-flush behavior

On `StreamDone`, commit any residual tail, do one final
re-render, store as finalized. Add the width-change-mid-
stream test.

- Exit: resize during active stream produces correct output
  (no lost content, no double-committed lines).

## Test plan

- **Unit (PR-A):** the collector's commit/tail invariants at
  many delta-shape edge cases. Mixed line endings
  (`\n`, `\r\n`), deltas that end exactly on `\n`, deltas
  with no `\n` at all, empty deltas.
- **Byte-identical final render (PR-B):** feed 100 deltas
  that together form a known markdown document; after
  finalization, the rendered block must equal the render of
  the full doc passed at once.
- **Width change mid-stream (PR-C):** 50 deltas, resize at
  delta 25, 50 more deltas. Final render matches the same
  content rendered at the new width from scratch.
- **Allocation regression:** `dhat` feature flag assertion —
  deltas per second cap at `wrap_text(tail)` allocations, no
  markdown-parser allocations until a new line commits.

## Risks

- **Unclosed fences in tail.** If a delta opens a ``` code
  fence and the next `\n` is inside the fence, we commit a
  line that reads mid-fence as prose. Mitigation: only commit
  lines **up to but not including** an unclosed fence.
  Detect by regex (`^```\w*$` without a matching close) when
  deciding how far the commit goes. This is a real edge case
  users will hit on streaming `fn foo() { … ```rust …` in
  agent output.

  Fallback if that's too fiddly: commit lines greedily but
  hold the last ≤3 lines in tail, so any fence-opening line
  near the end has a buffer. Cheaper, almost as correct.

- **Width-change mid-stream.** `committed_lines` cache is at
  the old width; tail is at neither. PR-C must test this.

- **Interaction with Plan 04.** Plan 04's drain-and-batch
  still applies — we batch N deltas per frame, then feed
  them all through `push_delta` at top-of-frame. The
  collector handles the batched input naturally.

- **Finalization subtleties.** The final block must look
  identical to a never-streamed block for cache purposes.
  Don't leave a `StreamingMarkdownBuffer` stored where a
  `Block` is expected — convert at finalization.

- **Added complexity.** This is the biggest architectural
  change in the plan set. If Plans 01–06 close the gap,
  we may not need this one. Stage accordingly (land 01–06
  first, measure, then decide on 08).

## Exit criteria

- [ ] All three PRs landed.
- [ ] `stream_into_static_600` p50 frame time ≤ 50% of the
      Plan 03 baseline.
- [ ] Allocation count: markdown parser allocations per
      second during a stream ≤ newline-rate (not
      delta-rate).
- [ ] Width-change-mid-stream regression test passes.
- [ ] Unclosed-fence edge case handled (test included).
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.

## Deferred

- **Type-writer pacing.** Codex paces committed lines for
  visual effect; we don't want that. If we ever do, add a
  `commit_queue` and a tick-drain step — the structure here
  supports it cleanly.
- **Reusing a third-party crate.** `tui-markdown` is a PoC
  with fewer features than our renderer; do not adopt. If
  a future well-maintained `ratatui-markdown-stream` crate
  appears, revisit.
- **Incremental pulldown-cmark parsing.** pulldown-cmark
  doesn't support it and it isn't needed — full re-parse per
  commit is cheap enough at the line-commit cadence.
