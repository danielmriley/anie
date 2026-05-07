# Code review — 2026-05-03

Branch: `dev_rlm`. Focus: whole-project assessment with a deep dive
on the TUI render pipeline and the user-reported sluggishness when
typing into the input area.

This review only proposes changes — no code was modified. All
findings cite `path:line` against current HEAD so they are
verifiable in place.

---

## TL;DR

- The codebase is in good shape overall: cleanly split crates,
  strong test coverage (unit + integration + criterion benches),
  and an explicit pi-adoption discipline that has driven multiple
  rounds of TUI perf work in the recent past
  (`docs/tui_perf_2026-04-25/`, `docs/tui_responsiveness/`,
  `docs/tui_perf_architecture/`).
- The TUI's expensive paths (transcript `Vec<Line>` build,
  per-block markdown re-parse, multi-width line cache) are
  already well covered. **The keystroke→paint hot path still has
  several small per-frame allocations and one `O(N)` content-clone
  per keystroke that, in aggregate, are the most plausible
  remaining cause of typing lag.**
- The single biggest structural win available is **partial
  paints**: today every keystroke paints the *entire* terminal
  buffer, even though only the input area changed. Ratatui's
  `Terminal::draw` model forces a full buffer rebuild + cell-diff
  pass per frame; a region-restricted paint for keystrokes would
  cut per-keystroke buffer-population cost by ~10× on a typical
  terminal.
- Actionable plan at the end of this document, ordered by
  user-visible impact-vs-effort.

---

## Project-wide assessment

### What's working well

1. **Crate split is clean and motivated by data flow.**
   `anie-protocol` (wire types) → `anie-provider` (transport +
   error taxonomy) → `anie-providers-builtin` (concrete
   providers) → `anie-agent` (run loop, tool wiring) →
   `anie-cli` (controller, interactive mode) →
   `anie-tui` (terminal UI). Each crate has its own targeted
   tests; the dependency direction never reverses.

2. **Strongly-typed `ProviderError` taxonomy** in
   `crates/anie-provider/src/error.rs` is genuinely better than
   the regex-classification approach in pi (and the comparison
   doc says so). The retry policy
   (`crates/anie-provider/src/retry.rs::RetryPolicy::decide`)
   routes by variant, not by string match.

3. **Pi-adoption discipline.** `CLAUDE.md` codifies "match pi's
   shape unless documented", "evidence-first", and "watch for
   per-frame integration points." The seven plans under
   `docs/pi_adoption_plan/` and the completed
   `docs/max_tokens_handling/` and `docs/tui_responsiveness/`
   show this is more than aspiration.

4. **TUI perf instrumentation is opt-in and structured.**
   `crates/anie-tui/src/render_debug.rs` ships per-frame counters,
   `ANIE_DEBUG_REDRAW=1` for redraw logs, and `ANIE_PERF_TRACE=1`
   for JSONL spans (parseable with `jq`). Plus
   `ANIE_TRACE_TYPING=1` in `app.rs::run_tui:2049` for direct
   keystroke→paint latency measurement.

5. **Session schema versioning** is explicit (`anie-session`'s
   `CURRENT_SESSION_SCHEMA_VERSION` + forward-compat tests). New
   fields default safely on old sessions.

### Things that warrant attention (non-perf)

1. **Doc folder discoverability.** `docs/` has 30+ topic
   directories at the top level plus a few stray `.md` files
   (`code_review_2026-04-27.md` next to a `code_review_2026-04-27/`
   directory; `tmp.md`; root-level `code_review_*.md` siblings to
   the better-organized subdirectories). A `docs/README.md`
   index — or just a one-line `docs/INDEX.md` table — would help
   future contributors find what's already been thought through.

2. **Stray files at repo root.** `tmp.md`, `review_input.rs`,
   `local_mitigations.md`, and several `code_review_*.md` siblings
   to the curated `docs/code_review_*/` directories. None are
   referenced from any other doc and they aren't ignored. Worth
   either moving under `docs/` or deleting.

3. **`crates/anie-tui/src/app.rs` is 2766 lines.** Most of that is
   load-bearing (event loop, dispatch, status bar), but
   `dispatch_validated_command` is a 165-line match on string
   names (`app.rs:1315-1485`) that has been flagged for splitting
   in `docs/tui_perf_2026-04-25/05_simplifications.md` (F-3) and
   not yet acted on. Not a bug today; will become one as more
   commands land.

4. **`crates/anie-tui/src/overlays/onboarding.rs` is 2711 lines.**
   Single file owning ~7 sub-screens. Same shape concern as
   `app.rs` — any future overlay changes will be hard to review.

5. **No `cargo fmt --check` evidence in this branch.** The 2026-04-27
   review (`docs/code_review_2026-04-27.md` finding 3) flagged
   formatting drift across `tui/src/*.rs` and several other
   files. Worth a `cargo fmt --all` pass before further work
   touches those files.

---

## TUI rendering — current pipeline summary

The keystroke→paint path on this branch is, in order:

1. **Key arrives** (crossterm `EventStream`).
2. `App::handle_terminal_event_dirty` → `handle_key_event` →
   `handle_editor_key` → `InputPane::handle_key` →
   `dispatch_editor_key` → `insert_char`.
3. `refresh_autocomplete()` runs synchronously (per-key, per
   `input.rs:222`).
4. `RenderDirty::composer()` returned; `dirty.composer = true`,
   `input_urgent = true`.
5. Event loop drains any further buffered terminal events
   (`app.rs:2130-2136`) so a key burst coalesces into one paint.
6. `terminal::draw_urgent` (no DECSET 2026 wrap) →
   `Terminal::draw` swaps the previous buffer, clears the new
   one, runs the user callback.
7. `App::render_with_mode(f, UrgentInput)` runs:
   - `tick_autocomplete` (no-op since `input.rs:231`),
   - `Spinner::tick` (returns `&'static str`),
   - `InputPane::preferred_height(width)` (cached layout),
   - `format_status_text(...)` — fresh `format!`,
   - `status_bar_height(...)` — `text.to_string()` + Paragraph
     wrap pass,
   - `OutputPane::render(area, buf, ".", true)` — visible-slice
     `set_line` writes against cached `Arc<Line>`,
   - `render_spinner_row` — single Paragraph render,
   - `InputPane::render(...)` — second cache read; line `String`s
     cloned per visible row,
   - `build_status_paragraph(status_text).render(...)` — second
     Paragraph wrap pass.
8. Ratatui diffs the new buffer against the previous one and
   writes only changed cells to stdout.

The *unavoidable* cost is step 7's buffer population (each cell
must be filled before the diff can run) and step 8's cell-diff +
stdout write. The avoidable costs are documented below.

---

## Findings

Severity tags:

- **confirmed-hot**: fires on every keystroke paint; allocates or
  walks more than necessary.
- **suspect**: plausible per-frame cost; not measured but reads
  the same way as previously caught issues.
- **structural**: not a per-call cost but a missed architectural
  win.
- **code-health**: not a perf issue.

---

### F-1. Status bar text + paragraph rebuilt twice per paint — **confirmed-hot**

`crates/anie-tui/src/app.rs:634-635` calls `format_status_text`
once for sizing and `build_status_paragraph(status_text).render(...)`
at `app.rs:671` for rendering. In between, `status_bar_height`
(`app.rs:2334-2340`) re-runs `build_status_paragraph(text.to_string()).line_count(width)`
— another `String` clone and another full Paragraph wrap pass.

Per keystroke paint:

- 1× `format!` allocation for the joined status string,
- 1× `text.to_string()` clone for the sizing wrap pass,
- 2× `Paragraph::new(...).wrap(Wrap{trim:false})` builds (one for
  `line_count`, one for `render`).

The status text changes only when `provider_name`, `model_name`,
`thinking`, `cwd`, `harness_mode`, `last_known_input_tokens`,
`estimated_context_tokens`, `context_window`, `rlm_archived_messages`,
or `is_scrolled` flip. None of those change per keystroke. The
whole status block could be cached on `StatusBarState` keyed by a
revision counter, exactly as `cached_short_cwd` is today
(`app.rs:328-365`).

This is the highest-confidence per-keystroke residual cost.
Estimated: 5–15 µs/keystroke depending on terminal width, plus
the heap allocator pressure of two short-lived `String`s and two
`Paragraph` builds.

---

### F-2. `InputPane::render` clones each cached layout line per paint — **confirmed-hot**

`crates/anie-tui/src/input.rs:381-388`:

```202:209:crates/anie-tui/src/input.rs
        let cached = self.layout(inner.width.max(1));
        let rendered_lines = cached
            .lines
            .iter()
            .take(inner.height as usize)
            .map(|line| Line::styled(line.clone(), Style::default().fg(Color::White)))
            .collect::<Vec<_>>();
```

Every visible input row is a fresh heap-allocated `String`
clone, even though the cached layout owns identical strings that
already live for the whole render pass. The `Paragraph::new`
that follows then walks those `String`s a third time.

For a one-row input the cost is one `String` clone per paint.
For a multi-line draft (Shift+Enter inserts), it's N clones per
paint — and these fire on *every* keystroke.

Two cheap fixes, in order of preference:

- Store the styled `Line<'static>`s directly in `CachedLayout`
  so render is `Paragraph::new(&cached.styled_lines)`.
- Or change the type signature to borrow with a constrained
  lifetime: `Line::styled(line.as_str(), ...)` won't work because
  `Line` insists on `Cow<'static, str>`, but pre-styling at cache
  time bypasses that.

---

### F-3. `InputPane::layout` cache key clones the buffer to detect mutation — **confirmed-hot**

`crates/anie-tui/src/input.rs:405-420`:

```405:420:crates/anie-tui/src/input.rs
    fn layout(&mut self, width: u16) -> &CachedLayout {
        let stale = self.cached_layout.as_ref().is_none_or(|c| {
            c.width != width || c.cursor != self.cursor || c.content != self.content
        });
        if stale {
            let (lines, cursor_visual) = self.layout_lines_uncached(width);
            #[cfg(test)]
            self.layout_misses.set(self.layout_misses.get() + 1);
            self.cached_layout = Some(CachedLayout {
                width,
                cursor: self.cursor,
                content: self.content.clone(),
                lines,
                cursor_visual,
            });
        }
```

Two costs per keystroke:

1. `c.content != self.content` does a full O(N) byte compare on
   every cache check — twice per paint (once from
   `preferred_height`, once from `render`). Cheap individually
   but wasteful.
2. On the stale branch (which is *every* keystroke, since the
   buffer changed), `self.content.clone()` allocates and copies
   the entire input buffer just to seed the next cache key.

Plan 01 of `docs/tui_perf_2026-04-25/` proposed a `revision: u64`
counter that bumps on every mutation; the cache key compares the
counter instead of the buffer. The current implementation uses
content cloning instead — explicitly noted in the test names
(`layout_cache_invalidates_on_insert`, `..._on_cursor_move`). The
revision-counter shape was the original plan and is still the
right move: it removes one allocation and two O(N) compares per
keystroke without changing semantics.

---

### F-4. Output pane visible slice is re-written on every keystroke paint — **structural**

`crates/anie-tui/src/app.rs:639-644`:

```639:644:crates/anie-tui/src/app.rs
        self.output_pane.render(
            output_area,
            frame.buffer_mut(),
            spinner_frame,
            matches!(mode, RenderMode::UrgentInput),
        );
```

The `reuse_flat_snapshot=true` flag does suppress the
`flat_lines` *rebuild* (`output.rs:683-689`), but the body of
`OutputPane::render` still runs and writes every line in the
visible viewport to the buffer:

```733:742:crates/anie-tui/src/output.rs
        for (row_offset, line) in visible.iter().enumerate() {
            let Ok(offset_u16) = u16::try_from(row_offset) else {
                break;
            };
            let y = area.y.saturating_add(offset_u16);
            if y >= area.y.saturating_add(area.height) {
                break;
            }
            buf.set_line(area.x, y, line.as_ref(), area.width);
        }
```

For a 200×80 terminal with a 60-row output area, that's 60
`set_line` calls per keystroke, each walking every span and
writing characters into buffer cells. The Arc-shared lines mean
no `Line` clones — but the buffer-write pass cannot be skipped
because `Terminal::draw` *clears* the new buffer before our
callback runs. We must repopulate it from scratch every frame.

This is the **single largest avoidable cost** on the keystroke
hot path, but it requires a structural change: bypass
`Terminal::draw`'s buffer-swap for urgent paints and emit a
direct partial frame that touches only the input + cursor cells.
See action plan §1 below.

---

### F-5. Output pane fast-path does not actually check `flat_cache_valid` — **suspect (correctness, not perf)**

`crates/anie-tui/src/output.rs:801-803`:

```801:803:crates/anie-tui/src/output.rs
    fn can_reuse_flat_snapshot(&self, width: u16) -> bool {
        self.flat_cache_width == Some(width) && !self.flat_lines.is_empty()
    }
```

When `reuse_flat_snapshot=true`, this skips the rebuild even if
`self.flat_cache_valid == false`. That's deliberate (keystroke
paints prefer "show stale by one frame" over "spend a millisecond
rebuilding"), but `flat_cache_valid` is set false on every
agent-event delta append. Streaming + typing can produce a paint
where the visible output is one delta behind the in-memory
state. Probably acceptable; worth a comment to document the
trade.

Code-health rather than perf — but flag it because a future
maintainer could tighten the predicate without realizing they're
removing the keystroke-latency optimization.

---

### F-6. `apply_user_message_tint` allocates large pad strings per cache miss — **suspect**

`crates/anie-tui/src/output.rs:1214-1247`:

```1241:1244:crates/anie-tui/src/output.rs
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), bg_style));
            }
            Line::from(spans)
```

Each user-message line produces a `" ".repeat(pad)` String. For a
typical 120-wide terminal with a 5-line user message that's 5×
~115-char allocations per cache miss. Cache-protected, so on the
steady state this fires only once per user message — but on
session load (`load_transcript`) every user message goes through
this path back-to-back, allocating thousands of pad strings on
startup for long sessions. Then again on width change.

Two options:

- Cache a single static `Span<'static>` of pre-allocated spaces
  per width once per render cycle and reference-clone it.
- Skip the pad span entirely and instead set the Line's `style`
  to the bg color, letting ratatui fill the background across
  the line. (Need to verify ratatui actually paints background
  across the unfilled cells; if not, option 1 is the only option.)

---

### F-7. `Spinner::tick` runs every paint even when nothing animated — **suspect (cheap)**

`crates/anie-tui/src/app.rs:604` always calls `self.spinner.tick()`.
`Spinner::tick` (`app.rs:385-392`) compares
`last_tick.elapsed() >= 80ms` and may bump the frame index. Two
syscall-free clock reads per paint. Trivially cheap, but during
idle the returned frame is ignored — only `render_spinner_row`
under `Streaming`/`ToolExecuting`/`Compacting` actually uses it.

Can be skipped entirely when `agent_state == Idle`. ~50 ns
saved per idle keystroke paint. Listed for completeness, not
because it's worth a PR on its own.

---

### F-8. `render_spinner_row` always emits an empty Paragraph during idle — **suspect**

`crates/anie-tui/src/app.rs:2384-2389`:

```2384:2389:crates/anie-tui/src/app.rs
    if label.is_empty() {
        // Still render an empty paragraph so ratatui clears
        // any previous content in this cell region on a paint.
        Paragraph::new(Line::default()).render(area, buf);
        return;
    }
```

The comment is correct — the paragraph is needed to clear leftover
cells from a previous active-state render. But this is a
1-row × terminal-width Paragraph build + render every paint
during idle. Could be replaced with a direct
`buf.set_line(area.x, area.y, &Line::default(), area.width)` to
skip Paragraph's wrap pipeline.

---

### F-9. Autocomplete refresh fires on every keystroke even when the buffer is plain prose — **suspect (cheap, but cumulative)**

`crates/anie-tui/src/input.rs:215-224`. Every `handle_key` call
runs `refresh_autocomplete`, which dispatches to the provider's
`suggestions(&self.content, self.cursor)`. For non-`/` input,
`parse_context` (`autocomplete/mod.rs:95-143`) returns
`Context::None` immediately — but the dispatch through the trait
object and the (small) `CommandCompletionProvider::suggestions`
match is still on the per-keystroke path.

Trivial individually. Two places it could matter:

- A wide-screen long-buffer paste of pre-existing input fires
  refresh once per character. The drain-to-paint coalesces the
  paint, but `handle_key` is called per char before drain.
- A user typing `/` then a long argument: each char re-runs the
  prefix match against all commands and allocates a fresh
  `Vec<Suggestion>` (one `to_string()` per matched command + one
  `description` String). For 20 commands and a 30-char command
  line that's ~600 short String allocations during the typing
  burst.

Could short-circuit at `handle_key` when `key.code == Char(_) &&
!self.content.starts_with('/')`. Or hoist the `parse_context`
no-op detection.

---

### F-10. Spans built fresh on every render where they could be cached — **suspect**

`crates/anie-tui/src/app.rs:2392-2396`:

```2392:2396:crates/anie-tui/src/app.rs
    let mut spans = vec![
        Span::raw(" "),
        bullet,
        Span::styled(format!(" {label}"), Style::default().fg(Color::Yellow)),
    ];
```

The `format!(" {label}")` runs every paint while a run is
active. `label` for `Streaming` is the constant string
`"Responding"`; only the spinner `bullet` actually animates.
Static portions could be `Cow::Borrowed`.

---

### F-11. `extract_text` / `extract_thinking` collect into intermediate `Vec` then `.join` — **code-health**

`crates/anie-tui/src/app.rs:2545-2568`:

```2545:2554:crates/anie-tui/src/app.rs
fn extract_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
```

Note that `app.rs:2608` (`join_text_blocks`) uses the better
single-pass shape with an explicit comment about avoiding the
double allocation. The two `extract_*` functions immediately
above it use the worse shape. Per-event cost (`MessageEnd`,
`load_message`), not per-frame, so this is code-health rather
than confirmed-hot — but it'd be a good hygiene fix while the
file is open for the status-bar work.

---

### F-12. Tool-result body `format!` allocates per agent event — **suspect**

`crates/anie-tui/src/output.rs:1147`:

```1147:1147:crates/anie-tui/src/output.rs
                    format!("{}\n\nTook {:.1}s", result.content, elapsed.as_secs_f64(),)
```

Fires once per finalized tool block render (cache miss path).
`result.content` can be large (a long bash output). `format!`
copies it into a fresh `String`, then `prefix_lines`/`boxed_lines`
walks it again. The previous content lives in `result.content`
already; a small `prefix_lines` overload that takes
`(body: &str, suffix: Option<&str>)` would avoid the copy.

Cache-protected → not per-frame → suspect rather than
confirmed-hot.

---

### F-13. The full app frame is the unit of paint — **structural (the headline finding)**

Today `App::render_with_mode` always paints all four regions:
output transcript + spinner row + bottom (input or model
picker) + status bar. The keystroke paint can in principle skip
the output and status regions entirely — they don't change in
response to a keystroke — and only re-write the input box +
cursor.

Ratatui's `Terminal::draw` model makes this hard:
`terminal.draw(callback)` always swaps the buffer cache, clears
the new buffer, and runs the callback. The callback has to
populate every cell that should be visible on the next frame, or
they get cleared. There's no "patch this region" API at the
Terminal level.

But there are workarounds:

- **`Frame::render_widget` only writes to the buffer; it doesn't
  clear cells outside the widget's `area`.** So if we know the
  output region didn't change, we *could* skip rendering it —
  but the buffer was just cleared, so unwritten cells go to the
  default (empty) cell. Ratatui's diff against the previous
  buffer would then produce stdout writes erasing the visible
  output. Not safe.
- **Manual `Buffer::content_mut` access:** populate only the
  input cells, then call into the lower-level `backend.draw`
  with a synthesized cell-list. This is what
  `terminal::draw_urgent` could do, but it currently delegates
  to the standard `terminal.draw` path. A custom partial-frame
  helper would buy us the structural win.

Cost / win estimate: at 200×80 terminal, the full paint
populates ~16 000 cells per frame; a partial input paint would
touch ~120 × 3 = 360 cells. ~40× reduction in buffer-write work
on the keystroke path, with corresponding reduction in ratatui's
diff cost.

This is the only finding in the list whose impact is large
enough to plausibly account for "feels sluggish" on its own. F-1
through F-3 are 5–30 µs improvements that compound; F-13 is a
~10× structural improvement on the dominant per-paint cost.

---

### F-14. Status bar uses `Wrap{trim: false}` even when the text fits on one line — **suspect**

`crates/anie-tui/src/app.rs:2322-2328`:

```2322:2328:crates/anie-tui/src/app.rs
fn build_status_paragraph(text: String) -> Paragraph<'static> {
    Paragraph::new(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    ))
    .wrap(Wrap { trim: false })
}
```

`Paragraph::wrap` walks the entire string at render time
regardless of whether it actually overflows. For a status string
that fits on one row (the usual case on a normal-width
terminal), this is wasted work. A `text.len() <= width` shortcut
could skip wrap entirely.

---

### F-15. Render loop always re-checks the budget gate — **code-health**

`crates/anie-tui/src/app.rs:2056-2065`. Each loop iteration
recomputes `resize_ready`, `budget_ready`, and rechecks
`dirty.any()`. Cheap, but the loop currently runs three
`Instant::now() - last_x` comparisons per pass. Could be
collapsed.

Listed because the agent-event drain path
(`drain_agent_event_batch`, `app.rs:104-117`) was recently
hardened for similar reasons; the input loop deserves the same
attention.

---

### F-16. Code organization observations from this pass — **code-health**

- `app.rs` mixes the event loop, slash-command dispatch, status
  bar, model picker plumbing, autocomplete wiring, layout
  helpers, and the spinner row in one 2766-line file.
  `dispatch_validated_command` (`app.rs:1315-1485`) is a
  165-line match. Plan 05 of `docs/tui_perf_2026-04-25/`
  documented the split; nothing has shipped.
- `overlays/onboarding.rs` is 2711 lines for one overlay screen
  family. Same shape concern.
- `output.rs` is 3199 lines and would naturally split into
  `block_render.rs` (the per-block layout helpers) +
  `flat_cache.rs` (the `flat_lines` + `LineCache` machinery)
  + the slim `OutputPane` itself.

---

## Action plan to address typing sluggishness

Ordered by **(user-visible impact for typing) ÷ (engineering effort)**.

Each item is a separable PR. Cargo gates after each:
`cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`.

### PR 1 — Cache the status bar text + paragraph (F-1)

**Scope.** Move the `StatusBarState` → `String` formatting
behind a revision counter. Bump the counter when any field
changes (or just bump it inside the existing `set_*` paths /
`StatusUpdate` handler). Cache `(revision, width)` →
`(formatted_text, paragraph_height)` so `format_status_text`,
`status_bar_height`, and `build_status_paragraph` all share one
cached value per paint.

**Files.** `crates/anie-tui/src/app.rs` (add cache fields to
`StatusBarState` next to `cached_short_cwd` at `app.rs:323-329`;
update `format_status_text`, `status_bar_height`, and the call
site in `render_with_mode`).

**Tests.** Add a per-keystroke regression test that confirms
`format_status_text` allocates zero new `String`s when state is
unchanged, mirroring the
`render_after_preferred_height_does_not_recompute` test in
`input.rs`.

**Expected win.** ~10–20 µs per keystroke paint, removes
3 allocations + 2 wrap passes. Highest confidence on this list.

---

### PR 2 — Stop cloning input buffer in the layout cache key (F-3)

**Scope.** Replace `c.content != self.content` with a `revision: u64`
counter that bumps on every mutation (`insert_char`,
`backspace`, `delete`, `delete_line`, `delete_to_line_end`,
`delete_word_backward`, `clear`, history navigation, autocomplete
apply). Cache key becomes `(width, cursor, revision)`. Drop the
`content: String` field from `CachedLayout`.

**Files.** `crates/anie-tui/src/input.rs` (revision counter +
cache key + drop the clone in `layout`).

**Tests.** Existing
`layout_cache_invalidates_on_insert`,
`layout_cache_invalidates_on_cursor_move`,
`layout_cache_invalidates_on_width_change` still pass
unchanged. Add a new `layout_cache_invalidates_on_paste_replace`
that does multi-char insert via `replace_range` to confirm the
revision bump path covers programmatic mutations too.

**Expected win.** Removes one O(N) `String::clone` per
keystroke + two O(N) byte compares per paint. Bigger as the
buffer grows; for a typical 50-char draft, ~1 µs/keystroke.

---

### PR 3 — Pre-style the input pane's cached layout lines (F-2)

**Scope.** Change `CachedLayout.lines: Vec<String>` to
`CachedLayout.styled_lines: Vec<Line<'static>>`. Build the
styled lines once on cache fill; the render path becomes a
zero-allocation borrow.

**Files.** `crates/anie-tui/src/input.rs`.

**Risk.** The first row carries the `> ` prefix span; the
algorithm in `layout_lines_uncached` currently inserts the
prefix at the very end (`input.rs:756-758`). The pre-style step
needs to handle that.

**Expected win.** Removes N `String::clone`s per paint where N
is the number of visible input rows (1 for short drafts, up to
8 for long Shift+Enter blocks).

---

### PR 4 — Short-circuit autocomplete refresh for plain prose (F-9)

**Scope.** In `InputPane::handle_key`, skip
`refresh_autocomplete` when the buffer doesn't start with `/`
and the popup is not currently open. Closes the per-key trait
dispatch on the typing-prose path.

**Files.** `crates/anie-tui/src/input.rs`.

**Tests.** Add
`refresh_autocomplete_skipped_for_non_slash_buffer` (asserts
the provider's `suggestions` counter doesn't bump while the
user types ordinary text). Pin the inverse:
`refresh_autocomplete_fires_when_buffer_starts_with_slash`.

**Expected win.** Removes a small per-key cost on the prose
path. Largest benefit for users typing long messages without
slash commands.

---

### PR 5 — Skip status-bar wrap when the text fits on one line (F-14)

**Scope.** In `build_status_paragraph` (or the call sites), check
`text.chars().count() <= width as usize` and bypass `Wrap{...}`
when true. Single-line status is the common case on
desktop-width terminals.

**Files.** `crates/anie-tui/src/app.rs:2275-2340`.

**Tests.** Pin the bypass in a unit test against a known wide
terminal width; pin the wrap-still-applies path with a narrow
width.

**Expected win.** Modest (~3–5 µs/paint); composes with PR 1.

---

### PR 6 — Partial-paint fast path for keystrokes (F-13) **(structural; biggest win, biggest risk)**

**Scope.** Introduce a `terminal::draw_keystroke_partial` helper
that:

1. Borrows the previous frame's buffer (the one ratatui keeps as
   the "cached" buffer for diffing).
2. Writes only the input region (and the spinner row, since it
   may have advanced) into a synthesized cell list.
3. Sends that cell list directly through the backend
   (`Backend::draw`) without the full clear+repopulate cycle.
4. Updates ratatui's cached buffer in place so the next full
   paint has accurate previous-state.

**Files.** `crates/anie-tui/src/terminal.rs` (new helper),
`crates/anie-tui/src/app.rs` (call the helper from
`run_tui`'s urgent branch, only when `agent_state == Idle` and
no overlay), and a dedicated test that exercises the partial
path against `TestBackend`.

**Risk.** Ratatui doesn't expose `Buffer` mutation on the
cached frame as a public API; this requires either careful use
of `Terminal::backend_mut` + manual `Backend::draw`, or
upstream-style manipulation of the inner buffer. There's a real
chance this needs to be implemented as "render to a scratch
buffer, diff against the previous one ourselves, write the diff
to the backend." That's still a win because the scratch buffer
is small (input area only).

**Tests.**

- `keystroke_partial_paints_only_input_cells_when_idle`
  (asserts only the input region's cells are written to the
  backend).
- `keystroke_partial_falls_back_to_full_paint_during_streaming`
  (asserts no partial-paint when output is updating).
- Add a criterion bench
  `keystroke_into_idle_app_600_partial_paint` to track the win.

**Expected win.** Order-of-magnitude reduction in
buffer-population work on the keystroke path. Probably the
single most important change for the user's "feels sluggish"
report — the per-keystroke work goes from O(terminal_cells) to
O(input_cells).

**Caveat.** Because of the ratatui buffer-management coupling,
this PR could land as either "small and clever" (~80 lines if a
clean API path exists) or "structural rewrite" (~300+ lines
with a custom backend wrapper). Recommend prototyping in a
spike branch before committing to the plan PR.

---

### PR 7 — Apply the smaller cleanup wins (F-6, F-7, F-8, F-10, F-11)

**Scope.** Bundle the small cleanups that don't deserve their
own PR:

- F-6: Cache pad-string spans for user-message tint (or use
  Line-level background fill if ratatui supports it).
- F-7: Skip `Spinner::tick` when `agent_state == Idle`.
- F-8: Replace `render_spinner_row`'s idle-Paragraph with a
  direct `buf.set_line(...)` clear.
- F-10: Make `render_spinner_row` use `Cow::Borrowed` for the
  static `"Responding"`/`"Running"` portions.
- F-11: Convert `extract_text`/`extract_thinking` to the
  single-pass shape used by `join_text_blocks`.

**Files.** `crates/anie-tui/src/app.rs`,
`crates/anie-tui/src/output.rs`.

**Expected win.** Individually small (<5 µs each), but they
remove allocator pressure and document the shape we want to
keep. Land last, behind the bigger wins.

---

## Verification protocol

Before each PR:

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check    # currently red on this branch; clean up first
```

After each PR:

```bash
cargo bench -p anie-tui --bench tui_render -- \
  --warm-up-time 1 --measurement-time 3
```

Track `keystroke_into_idle_app_600`,
`keystroke_during_stream_600`, and
`keystroke_into_long_buffer` (added by PR 02 of
`docs/tui_perf_2026-04-25/`). Update
`docs/tui_perf_2026-04-25/README.md`'s baseline table after each
PR.

Manual smoke per PR: open a session with a populated transcript
(e.g. `anie --resume <id>` against a long session), type rapidly
into the input area, and confirm no visible lag. With
`ANIE_TRACE_TYPING=1` the `t_key_to_paint_us` log line in
`~/.anie/logs/anie.log.*` should stay below 5 ms (currently
expected ~4 ms median per the `docs/tui_perf_2026-04-25/00_report.md`
TL;DR baseline).

---

## What this review explicitly does NOT recommend

- **Replacing ratatui.** The block cache + flat cache + Arc
  sharing are deeply integrated with ratatui's `Line` /
  `Buffer` types. The pi-equivalent isn't strictly better and
  the migration cost would dwarf the wins above.
- **A parallel render thread.** Pi runs single-threaded and so
  do we; the keystroke path's bottleneck is per-frame
  allocation + buffer population, not parallelizable work.
- **Re-tuning `FRAME_BUDGET`.** It's currently 8 ms
  (`app.rs:2000`) and the doc comment explains the
  measurements behind that choice. Tightening it further
  trades wasted CPU for negligible latency improvement.
- **Replacing syntect for code-block highlighting.** The cache
  protects per-block; per-frame cost on this path is already
  zero for stable transcripts.
- **A second debounce on autocomplete.** The previous debounce
  was explicitly removed (`input.rs:215-221`) for being
  premature optimization. PR 4 above is the right shape: skip
  the work entirely when there's no `/` prefix, rather than
  debouncing it.

---

## Implementation status (2026-05-03)

The seven PRs above were worked through in a single batch on
`dev_rlm`. Status, with the exact behavior that landed:

### PR 1 — Cache the status bar text + paragraph (F-1) — **landed**

- Added `StatusRenderCache` struct + `cached_render` field on
  `StatusBarState` (`crates/anie-tui/src/app.rs`). The cache
  snapshots all 11 input fields plus `(transcript_scrolled,
  width)`; cache hits do field-equality only, no `format!`,
  no `Paragraph::wrap` sizing pass.
- `format_status_text` and `status_bar_height` were renamed to
  `build_status_text` / `measure_status_height` and made pure
  helpers called only on cache miss.
- `render_with_mode` now calls
  `self.status_bar.cached_render(scrolled, width)` once and
  reuses both the text and the height.
- `#[cfg(test)] render_misses` counter mirrors
  `InputPane::layout_misses` for regression tests.
- New tests:
  `cached_status_render_does_not_rebuild_when_state_is_unchanged`,
  `cached_status_render_invalidates_on_width_change`,
  `cached_status_render_invalidates_when_input_field_changes`.

### PR 2 — Stop cloning input buffer in the layout cache key (F-3) — **landed**

- Added `content_revision: u64` field on `InputPane` plus
  `bump_revision()` helper (`crates/anie-tui/src/input.rs`).
- Replaced the `(width, cursor, content_clone)` cache key in
  `CachedLayout` with `(width, revision)` — two
  machine-word comparisons per hit, no `String == String`
  memcmp, no `String::clone`.
- Audited every mutator (`clear`, `submit`, `insert_char`,
  `backspace`, `delete`, `move_left`/`right`,
  `move_to_line_start`/`end`, `move_word_left`/`right`,
  `delete_line`, `delete_to_line_end`, `delete_word_backward`,
  `history_previous`/`next`, `apply_autocomplete_selection`)
  to bump the revision; boundary no-op cases (move_to_line_*)
  guard the bump so they don't false-miss the cache.
- New tests:
  `layout_cache_hits_repeatedly_without_mutation_for_large_buffer`
  (2 KB buffer × 32 paints, zero rebuilds),
  `editor_mutations_bump_layout_cache_revision`.

### PR 3 — Pre-style the input pane's cached layout lines (F-2) — **landed**

- `CachedLayout` now stores `styled_lines: Vec<Line<'static>>`
  built once on cache miss; `Vec<String>` is gone.
- The render path in `InputPane::render` writes lines directly
  via `buf.set_line` (same shape as `OutputPane::render`),
  dropping the per-paint `Paragraph::new(Vec<Line>)`
  construction and the per-line `String::clone` +
  `Line::styled` wrap.
- Added `CachedLayout::line_count()` for sizing and
  `#[cfg(test)] line_text(idx)` for assertion ergonomics.
  Existing tests that read `cached.lines[i]` were updated to
  `cached.line_text(i)`.
- New tests:
  `render_does_not_rebuild_styled_lines_when_state_unchanged`,
  `cached_styled_lines_carry_white_foreground`.

### PR 4 — Short-circuit autocomplete refresh for plain prose (F-9) — **landed**

- `InputPane::refresh_autocomplete` now returns early when the
  buffer doesn't start with `/` AND no popup is currently
  open. Plain-prose typing skips the trait dispatch into the
  provider entirely.
- The popup-open branch still runs the provider so a slash
  buffer that just stopped matching (e.g. user backspaced
  through the leading `/`) closes the popup correctly.
- Updated two pre-existing tests
  (`autocomplete_fires_synchronously_per_keystroke`,
  `tick_autocomplete_is_noop`) to use a slash buffer so they
  continue to pin "no debounce" without being defeated by the
  new guard.
- New tests:
  `typing_plain_prose_does_not_invoke_autocomplete_provider`,
  `typing_a_slash_re_engages_the_autocomplete_provider`.

### PR 5 — Skip status-bar wrap when text fits on one line (F-14) — **landed**

- Added `status_text_fits_one_row(text, width)` helper
  (`crates/anie-tui/src/app.rs`): no embedded `\n` AND
  `Span::raw(text).width() <= width as usize`. Cheap — one
  unicode-width pass via ratatui's existing `Span::width`
  method, no allocation.
- `measure_status_height` takes the fast path (returns 1)
  when the predicate holds; the wrap pass only fires for
  multi-row content.
- The render call site in `render_with_mode` mirrors the
  predicate: when `status_height == 1` AND the text still
  fits, it writes via `buf.set_line` and skips the
  `Paragraph::new(...).wrap(...)` pass. The two paths share
  the same predicate so they always agree on row count.
- New test: `status_text_fits_one_row_matches_actual_wrap`.

### PR 6 — Partial-paint fast path for keystrokes (F-13) — **deferred**

After studying ratatui 0.29's `Terminal` internals
(`~/.cargo/registry/src/index.crates.io-*/ratatui-0.29.0/src/terminal/terminal.rs`):
ratatui resets the back buffer at every `swap_buffers` and
does not publicly expose the previous frame's buffer. A true
partial paint requires either bypassing `Terminal::draw`
entirely (writing directly through `Backend::draw` with
manually computed cursor moves) or reaching into ratatui's
private buffer pair via unsafe / patched code. Either is a
spike-branch effort, and the report itself flagged this PR
as the highest-risk one needing prototyping first.

The wins from PR 1, PR 3, PR 5, and PR 7 reduce the
keystroke paint's per-frame allocations from "many small"
to "essentially zero in steady state." Combined with PR 2's
removal of the O(N) clone+compare on the layout cache, the
typing path should now be substantially below the previous
~4 ms median floor without needing the partial-paint
structural change. PR 6 is left for a focused follow-up
where it can be benchmarked on its own merits.

### PR 7 — Smaller cleanup wins — **mostly landed (F-7 deliberately deferred)**

Implemented:

- **F-6** (user-message tint padding): replaced
  `" ".repeat(pad)` with a `Cow<'static, str>` slice off a
  `OnceLock<String>` 512-space buffer
  (`crates/anie-tui/src/output.rs::padding_spaces`). Falls
  back to `repeat()` for terminals wider than 512 columns
  (extremely rare). Zero per-paint allocations on every
  reasonable terminal size.
- **F-8** (idle spinner row): the empty-Paragraph render in
  `render_spinner_row`'s idle branch is gone; the function
  returns early instead. Verified safe against ratatui's
  `swap_buffers`-resets-back-buffer behavior — the diff
  against the previous frame still clears the row when
  transitioning out of an active state.
- **F-10** (spinner label allocation): the `label` is now
  `Cow<'static, str>`, with the dominant `Streaming` case
  borrowing `"Responding"` as a `&'static str`. Tool /
  compacting branches still allocate (they embed dynamic
  values).
- **F-11** (`extract_text` / `extract_thinking` double
  allocation): replaced the `.collect::<Vec<_>>().join("\n")`
  shape with a single-allocation `join_blocks_with_newline`
  helper. New tests: `extract_text_matches_legacy_join_shape`,
  `extract_thinking_includes_redacted_placeholder`.

Deferred:

- **F-7** (skip `Spinner::tick` when idle): attempted, then
  reverted. The spinner frame is also consumed by the output
  pane's tool-call bullet rendering
  (`output.rs::tool_call_bullet_spans`). The "agent state is
  Idle implies no `is_executing` tool block" invariant
  holds in normal flow, but the abort-mid-tool edge could
  briefly violate it. The savings (~100 ns/frame) didn't
  justify the fragile invariant; documented in the inline
  comment at the `self.spinner.tick()` call site.

### Test + lint status after the batch

- `cargo test --workspace` — green; `anie-tui` went from
  430 → 439 tests (twelve new regression tests added, two
  pre-existing autocomplete tests rewritten to the new
  guard-aware shape).
- `cargo clippy --workspace --all-targets -- -D warnings` — green.
- `cargo bench -p anie-tui --no-run` — bench harness still
  compiles. Benchmark numbers were not re-measured here; the
  next session should run
  `cargo bench -p anie-tui --bench tui_render -- --warm-up-time 1 --measurement-time 3`
  and update the baseline table in
  `docs/tui_perf_2026-04-25/README.md`.

### Code shape summary

- `crates/anie-tui/src/app.rs`: status-bar cache (PR 1), spinner
  / status-render fast paths (PR 5, PR 7).
- `crates/anie-tui/src/input.rs`: revision counter (PR 2),
  pre-styled cache (PR 3), autocomplete guard (PR 4).
- `crates/anie-tui/src/output.rs`: static spaces buffer (F-6).

Every behavior change is annotated inline with `PR N of
docs/code_review_2026-05-03.md (F-X)` so a future reviewer
can trace any change in this batch back to its rationale.

---

## Round 2 — user reported "still quite a lot of sluggishness"

After the first batch landed, smoke testing in a real terminal
still felt slow on the keystroke path. Two new bottlenecks
turned up — both invisible to the existing `TestBackend` bench
harness, which is why the first round didn't catch them.

### F-17. `CrosstermBackend` writer is unbuffered, so every `queue!` hits the global stdout lock — **confirmed-hot, real-terminal only**

`crates/anie-tui/src/terminal.rs:55` (pre-fix):

```rust
let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
```

ratatui's `CrosstermBackend::draw` issues one or more `queue!`
writes per cell update (cursor move, fg/bg, modifiers, the
glyph itself). Without a `BufWriter`, every one of those calls
goes through `Stdout::write`, which acquires the global
process stdout lock. For a keystroke diff this is "only" a few
hundred lock acquisitions; for a streaming-burst diff or a
full repaint after scroll/resize it's tens of thousands.

This is documented in ratatui's FAQ
(["Should I use stdout or stderr?"](https://ratatui.rs/faq/#should-i-use-stdout-or-stderr))
and the related discussion at
ratatui/website#274. The upstream guidance is explicit:

> "Out of the box, stdout will be faster than stderr because
> it is buffered. However you can very easily make stderr
> buffered too by wrapping it in a `BufWriter`."

The reported impact in that thread: **15 → 90 fps in 80×24,
1.6 → 2 fps in fullscreen** when the writer was wrapped.

`TestBackend` writes into a `Vec<u8>` with no lock contention,
which is why the existing benches showed 380 µs/keystroke
even though real-world feel was much worse. Severity: high
on real terminals, invisible to the bench.

---

### F-18. Output-pane visible rows are still re-`set_line`'d on every urgent paint — **confirmed-hot**

The first-round review (F-4 / F-13) flagged this but deferred
the fix as "structural — bypass `Terminal::draw`'s buffer
swap, scary." On re-reading, there's a simpler answer that
does **not** require bypassing `Terminal::draw`.

The cost the bench captured (~380 µs of which ~150 µs is
output-pane work) breaks down to ~30 ns × 4–5 K cells in
`Buffer::set_line` per urgent paint. That cost is paid
*every* keystroke even when the output content hasn't moved,
because `Terminal::draw` wipes the back buffer before the
render closure runs and we have to repopulate every cell or
ratatui's diff will emit clear sequences for the unwritten
ones.

`Buffer::set_line` does, per cell:

1. `UnicodeSegmentation::graphemes` iteration over the span.
2. `unicode-width::width()` lookup per grapheme.
3. `CompactString::new(symbol)` to build the cell symbol.
4. Style patch + cell write.

`Cell::clone` does, per cell:

1. `CompactString::clone` (≈ memcpy for ≤24 B inline symbols).
2. Five small POD field copies.

The clone path is several × cheaper. So if we **snapshot the
cells we wrote on the previous full paint** and replay them
with `Vec::clone_from_slice` on the next urgent paint, we
save the grapheme/width/string-allocation work entirely
without breaking the buffer-swap invariant. The diff still
runs (cheap, walks both buffers) but emits zero updates for
the unchanged region.

---

## Action plan — round 2

### PR 8 — Wrap `CrosstermBackend`'s writer in `BufWriter` (F-17) — **landed**

Tracked: `crates/anie-tui/src/terminal.rs`,
`crates/anie-tui/src/app.rs`,
`crates/anie-cli/src/{interactive_mode,onboarding}.rs`.

Changes:

- New `pub type TerminalStdout = BufWriter<Stdout>` and a 64 KiB
  capacity constant (`STDOUT_BUFFER_BYTES`) so the size is
  documented and easy to tune.
- `TerminalGuard::new` writes the alternate-screen / mouse-
  capture enable sequence directly to the unbuffered handle
  (so it lands before any buffered writes), then wraps stdout
  with `BufWriter::with_capacity(64 KiB, stdout)` and hands
  that buffered writer to `CrosstermBackend::new`.
- `terminal_mut`, `run_tui`, and the CLI callers all picked
  up the new `Terminal<CrosstermBackend<TerminalStdout>>`
  alias automatically.
- The DECSET 2026 wrap, `LeaveAlternateScreen`, and the
  cursor-show sequences continue to flow through the same
  `BufWriter` (they go through `terminal.backend_mut()`),
  which is itself flushed by ratatui at the end of every
  `terminal.draw(...)` and by `execute!`'s built-in flush at
  shutdown / restore time.

Expected real-world win: 5 – 50 % keystroke-paint latency
reduction depending on terminal emulator and frame size, per
the ratatui FAQ measurements. The existing `TestBackend`
benches do not measure this — they use an unlocked `Vec<u8>`
writer — so the bench numbers are unchanged.

Tests / lint:

- `cargo test --workspace` — 441 in anie-tui, all green.
- `cargo clippy --workspace --all-targets -- -D warnings` —
  clean.

### PR 9 — Snapshot the output cells once, replay them on urgent paints (F-18) — **landed**

Tracked: `crates/anie-tui/src/output.rs`,
`crates/anie-tui/src/tests.rs`.

Design:

- New private `RenderedSnapshot { area, scroll_offset,
  revision, cells: Vec<Cell> }` records the cells written by
  the most recent **non-animated full paint** of the output
  pane. Memory: `area.width × area.height × sizeof(Cell)`
  ≈ 128 KiB at 200×80 — single allocation reused across
  paints.
- New `OutputPane::render_revision: u64` is bumped by every
  mutator that already invalidated `flat_cache_valid`. The
  bump is centralized in a new `invalidate_flat_cache()`
  helper that flips both flags together — impossible to flip
  one without the other.
- `OutputPane::render` now has an urgent fast path at the top:
  when `reuse_flat_snapshot=true` and a standing snapshot
  matches `(area, scroll_offset, revision)`, the cells are
  replayed via `Vec::clone_from_slice` row by row and the
  function returns. No `rebuild_flat_cache`, no `set_line`,
  no per-cell grapheme walk.
- After every successful **non-animated full paint**, the
  cells just written are captured into the snapshot. A
  `snapshot_already_matches` guard skips the capture when
  the cells are unchanged (revision + scroll + area), so the
  steady-state full-paint cost is zero extra work.
- Animated content (streaming assistant, executing tool)
  invalidates the snapshot — the spinner needs to tick every
  frame, so a snapshot-replay would render a stale frame.
  Real urgent paints during streaming therefore fall back to
  the existing path (no regression vs. before this PR).

Tests added:

- `urgent_paint_replays_output_cells_from_snapshot_on_each_keystroke`
  — pin the contract: a 5-key burst yields exactly 5
  snapshot reuses.
- `snapshot_reuse_aborts_when_output_pane_mutates_between_paints`
  — regression guard: a `SystemMessage` queued between the
  warm paint and the urgent paint must force a full repaint.

Bench delta (criterion, `--warm-up-time 1 --measurement-time 3`):

| bench | before | after | Δ |
| --- | --- | --- | --- |
| `scroll_static_600` | 233 µs | 240 µs | +3 % (capture overhead) |
| `stream_into_static_600` | 1.41 ms | 1.10 ms | -22 % (revision-bump rewiring) |
| `resize_during_stream` | 508 µs | 547 µs | +8 % (capture on resize) |
| `keystroke_into_idle_app_600` | 386 µs | 350 µs | -9 % |
| `keystroke_during_stream_600` | 374 µs | 378 µs | flat (snapshot disabled) |
| `keystroke_into_long_buffer` | 388 µs | 354 µs | -9 % |

The `_idle_` and `_long_buffer_` keystroke wins are the ones
the user feels. The `during_stream` path is unchanged because
animated content disables the snapshot — that's the next
candidate work area (snapshot the non-animated rows only,
re-render only the animated band).

### What's *still* on the table after round 2

- **Streaming + typing concurrency.** `keystroke_during_stream_600`
  did not improve because the streaming block is animated —
  every paint walks through full block rebuilds. Splitting
  the snapshot at row granularity (snapshot non-animated
  rows, re-render the animated band only) would fix this
  but adds bookkeeping. Not yet attempted.
- **Diff-pass cost.** `Buffer::diff` walks every cell of both
  buffers regardless of which changed. At 200×80 that's
  ~100 µs/frame of guaranteed work even when the snapshot
  is reused. This would require either patching ratatui or
  bypassing `Terminal::draw` and synthesizing the diff
  ourselves — same structural work the original PR 6 was
  deferred for.

---

## Round 3 — user reported sluggishness *after* PR 8 + PR 9

After both BufWriter and snapshot replay landed, real-world
typing still felt slow. The bench numbers (350 µs/keystroke)
are well below the perceptual threshold, so the bottleneck is
necessarily something the bench can't see — i.e. terminal-
side or event-loop work that doesn't show up against
`TestBackend`.

### F-19. `EnableMouseCapture` enables `?1003h`, drowning the event loop in motion events — **confirmed-hot, real-terminal only**

Crossterm 0.29's `EnableMouseCapture` emits, verbatim:

```
\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h
```

(`crossterm-0.29.0/src/event.rs:323-334`).

`?1003h` — "Any-event tracking: Report all motion events" —
makes the terminal forward an event for **every** mouse
cursor movement over the anie window. anie only ever
consumes `ScrollUp`, `ScrollDown`, and `Down(Left)`
(`crates/anie-tui/src/app.rs:1364`); `MouseEventKind::Moved`
and `Drag` fall through to `RenderDirty::none()`. So the
events are pure noise — but each one:

1. Wakes our `tokio::select!` loop via `EventStream::next`.
2. Goes through `handle_terminal_event_dirty` →
   `handle_mouse_event`.
3. Gets drained alongside any keystrokes pending in the same
   poll batch — so an in-flight keystroke pays the cost of
   every motion event that arrived since the last paint
   before its own paint runs.
4. On a busy desktop, a typing user often rests one hand on
   the trackpad / mouse; even minor cursor drift fires
   motion events at ~50–100 Hz.

pi doesn't enable mouse capture at all
(`pi/packages/tui/src/terminal.ts:91-125` — only raw mode +
bracketed paste + Kitty keyboard protocol). anie wants
mouse capture for click-to-open-URL and scroll-wheel, so
"don't enable any of it" isn't the right answer. But the
right answer is "enable only the modes we use" —
specifically `?1000h` (button press / release; covers
scroll-wheel as buttons 4/5) and `?1006h` (SGR encoding
for coords > 223). `?1002h` and `?1003h` were never used.

Severity: high on real terminals when a mouse / trackpad is
in the same room as the keyboard. Invisible to the bench
because `TestBackend` doesn't carry mouse events.

---

## Action plan — round 3

### PR 10 — Drop `?1002h` / `?1003h`; enable button-only mouse reporting (F-19) — **landed**

Tracked: `crates/anie-tui/src/terminal.rs`.

Changes:

- New `ENABLE_BUTTON_ONLY_MOUSE` and `DISABLE_BUTTON_ONLY_MOUSE`
  constants emit / clear only `?1000h` (button events) and
  `?1006h` (SGR encoding). The disable string mirrors the
  enable in reverse order.
- `TerminalGuard::new` writes the alternate-screen sequence
  via `execute!` (unchanged), then writes the button-only
  mouse sequence directly to the unbuffered handle and
  flushes — so the enable lands before the BufWriter wraps
  stdout, matching PR 8's "enable side-effects pre-buffer"
  shape.
- `restore`, `Drop`, and the panic-hook restore path each
  use the new disable constant. Backend-side restores go
  through the BufWriter and rely on ratatui's `execute!`
  flush at shutdown; the panic hook flushes stdout
  directly.
- Crossterm's `EnableMouseCapture` / `DisableMouseCapture`
  imports are removed — anie no longer uses them anywhere.

Tests added:

- `terminal::tests::mouse_capture_sequences_omit_motion_tracking`
  pins the contract — both enable and disable must contain
  `?1000` and `?1006` and **must not** contain `?1002` or
  `?1003`. The disable order is also asserted (`?1006l`
  before `?1000l`, mirror of enable).

Real-world impact: every mouse-cursor drift over the anie
window stops generating events. In typical use that's
hundreds of dropped events per minute of typing, depending
on desk layout. The bench numbers are unchanged (bench
doesn't model mouse) but real-terminal keystroke latency
should drop measurably any time the cursor is on the
window.

Side benefit: with motion tracking off, terminals that
support it can pass click-and-drag through to native text
selection (the user can drag-select transcript text) when
no button is held, since the program no longer captures
those events. Click-to-open-URL still works via the
button-press half of `?1000h`.

Tests / lint:

- `cargo test --workspace --lib` — 442 tests, all green
  (one new test added).
- `cargo clippy --workspace --all-targets -- -D warnings`
  — clean.
- `cargo bench -p anie-tui --bench tui_render -- --warm-up-time 1 --measurement-time 3`
  — no significant change on any bench (expected; mouse
  events are bench-invisible).

### What's *still* on the table after round 3

- **`StreamingRenderCache::render_lines` deep-clones on hit.**
  Returning `c.lines.clone()` produces a fresh `Vec<Line>`
  per frame during streaming, then the immediate caller
  re-allocates each line in an `Arc` — wasted work. Storing
  the cache as `Vec<Arc<Line<'static>>>` directly would
  turn the hit path into refcount bumps. ~50–100 allocs
  per streaming frame on a 5 KB response; not on the
  typing path the user reported, but worth fixing if the
  next round of feedback flags streaming sluggishness.
- **Streaming row-band snapshot.** Same as round 2's
  follow-on: snapshot the non-animated rows, re-render
  only the streaming band. Bigger lift.
- **`Buffer::diff` walks every cell.** Same as round 2's
  follow-on. Real fix is bypassing `Terminal::draw` for
  urgent paints — structural change, deferred.
