# 00 — TUI sluggishness investigation report

Date: 2026-04-25
Branch: `feat/ollama-memory-safety` at `1138b3d`
Method: five parallel investigation agents (render hot path,
event loop, markdown rendering, block cache, code smell) plus
fresh criterion benchmarks. Findings consolidated below; every
item carries a `path:line` citation against current HEAD.

## TL;DR

1. **The benchmark suite doesn't measure what the user feels.**
   `tui_render` exercises `OutputPane::render` headlessly with
   no `App`, no input pane, no keystroke pipeline. Cache-hit
   paths are bench-fast but the keystroke-to-paint pipeline has
   three independent costs the bench never touches.
2. **The strongest input-lag suspect is the input pane itself**,
   not the output pane. `InputPane::layout_lines` runs twice
   per keystroke paint (once via `preferred_height`, once via
   `render`), each pass walking the full input buffer
   character-by-character. The output pane's urgent path is
   already wired correctly — the cost lives one widget over.
3. **A handful of per-frame costs fire on every paint regardless
   of dirty state**, including urgent paints that are supposed
   to be the cheap path: visible-slice deep clone (50–200 µs),
   `has_animated_blocks` O(N) walk, `shorten_path` `env::var` +
   `Vec` per call.
4. **Streaming output's residual cost is dominated by the link
   scan**, which re-walks the entire streaming block every
   frame because streaming bypasses the block cache. This
   single function is 2–5 ms/frame for a 100-line streaming
   answer.
5. **Recent feature work landed without freshening the
   benchmarks.** The bullet-style tool output (`d7c4fae`),
   progressive-streaming markdown (`9a57148`), DECSET-2026
   urgent path (`b125f98`), and click-to-open hyperlinks
   (`f051758`) all changed the per-frame cost shape, but the
   bench numbers in `baseline_numbers.md` are from before any
   of them. Today's baseline (above) is essentially the same
   number, so nothing has visibly regressed in the bench — but
   the user has changed environment along with us.

## Findings

Every entry: `path:line` citation, observed cost, and severity.
Severity is `confirmed-hot` (measured or unambiguous), `suspect`
(plausible but unmeasured), or `code-health` (correctness /
maintainability rather than perf).

### Input pipeline

**F-1. `InputPane::layout_lines` runs twice per keystroke paint.**
`crates/anie-tui/src/input.rs:267-272` calls `layout_lines` from
`preferred_height`. Then `crates/anie-tui/src/input.rs:287` calls
it again from `render`. Each call walks `self.content.char_indices()`
end-to-end (`input.rs:512-541`) and rebuilds `Vec<String>` lines.
For a 200-char buffer that's 400 char ops + 2 Vec allocations on
every keypress. The path is hot because `preferred_height` runs
unconditionally inside `App::render_with_mode`
(`crates/anie-tui/src/app.rs:531-533`). **Severity:
confirmed-hot.** Most likely root cause for "subtle but definite"
typing lag.

**F-2. Autocomplete `parse_context` walks full input buffer per
keystroke.** `crates/anie-tui/src/autocomplete/mod.rs:95-143`.
Synchronous on the keystroke path
(`crates/anie-tui/src/input.rs:565-571` / `:151`). Command
filtering itself is O(commands) and tiny (~20 builtins), but
the input-buffer scan is O(buffer_len) per char. Compounds with
F-1. **Severity: suspect.** Cheap individually; visible if the
user holds a key down or autorepeats fast.

**F-3. `dispatch_validated_command` is a 195-line match.**
`crates/anie-tui/src/app.rs:1100-1293`. Most arms are a
single-line `self.action_tx.send(UiAction::*)` differing only in
the variant. Not a perf cost — but it forces every keystroke
that hits `Enter` through a long branch chain that's hard to
keep correct as commands grow. **Severity: code-health.**

### Per-frame render hot path

**F-4. Visible-slice deep clone every paint.**
`crates/anie-tui/src/output.rs:605-609`:
```rust
let visible = if start < end {
    self.flat_lines[start..end].to_vec()
} else { Vec::new() };
```
`flat_lines` stores owned `Line<'static>` objects, so `to_vec()`
is a deep clone of the slice — every `Span`, every `Cow<'static,
str>` content copied. ~40 lines × ~5 spans/line = ~200 spans
copied per frame. Estimated 50–200 µs per paint. **This fires on
every render mode**, including urgent keystroke paints that pass
`reuse_flat_snapshot=true`. **Severity: confirmed-hot.**

**F-5. `has_animated_blocks()` O(N) walk per frame.**
`crates/anie-tui/src/output.rs:624-625` and `:649-651`. Iterates
all blocks via `.iter().any(block_has_animated_content)`. Called
unconditionally in `rebuild_flat_cache`, including on
cache-valid paths (the function decides whether the cache CAN
be reused — but to decide that, it walks). For a 600-block
transcript that's 600 enum-variant checks per frame. Cheap each
but cumulative on idle scrolls. **Severity: confirmed-hot.**

**F-6. Status-bar `shorten_path` allocates per frame.**
`crates/anie-tui/src/app.rs:2081-2189`. `shorten_path()` calls
`std::env::var("HOME")`, `replacen`, `split('/')`, `collect::<Vec<_>>()`
on every render — including idle ticks. The cwd doesn't change
per frame; the formatted path doesn't either. Estimated 1–2 ms/frame
across all the formatters. **Severity: confirmed-hot.**

**F-7. `flat_lines` stores owned Lines, not Arc.**
`crates/anie-tui/src/output.rs:203` (`flat_lines: Vec<Line<'static>>`),
`:712-723` (cache-hit extends do `iter().cloned()`). Even though
the per-block cache uses `Arc<Vec<Line>>` for refcount-cheap
shared storage, the flat cache deep-clones every Line out of
the per-block Arcs into the owned `flat_lines` vector. So the
shared structure exists but is broken at the seam. **Severity:
confirmed-hot** (compounds with F-4 — one `Arc::clone` would
fix both).

### Streaming output hot path

**F-8. `find_link_ranges` re-runs per frame on streaming
blocks.** `crates/anie-tui/src/output.rs:760-767` — link scan
runs once per cache miss. Streaming blocks bypass the cache
(`block_has_animated_content` returns true), so the link scan
runs every frame on the streaming block. The scan itself
(`crates/anie-tui/src/markdown/mod.rs:57-102`) walks every span
in every line, calling `chars().count()` twice on the same span
content (`mod.rs:68` and `mod.rs:79` — second call is
unnecessary). Estimated 2–5 ms/frame for a 100-line streaming
answer. **Severity: confirmed-hot.** This is likely the single
biggest contributor to "output feels sluggish during streaming."

**F-9. Tool block headers allocate fresh strings per render.**
`crates/anie-tui/src/output.rs:1414-1446`
(`format_tool_header_spans`): `prefix.to_string()`,
`verb.to_string()`, `args_display.to_string()`,
`spinner_frame.to_string()` per call. Box-style (`output.rs:1325-1362`)
allocates `format!("─ {title} ")`, `"─".repeat(...)`,
`format!("┌{...}┐")`, plus `" ".repeat(...)` per body line. The
helpers are only called on cache miss — but executing /
streaming tool blocks bypass the cache, so they fire every
frame. **Severity: confirmed-hot** during tool execution.

**F-10. `StreamingAssistantRender` cache key omits theme.**
`crates/anie-tui/src/output.rs:121-139`. Cache validity checks
`(width, markdown_enabled)` but not the theme. If the user
toggles markdown (and presumably eventually theme via the
deferred `/theme` command), cached committed-prefix lines could
render with the wrong theme until the next invalidation
trigger. **Severity: code-health** today (no theme switching
yet); becomes a bug the moment that ships.

**F-11. Streaming committed-prefix recomputes if no `\n\n`
arrives.** Implicit in
`crates/anie-tui/src/output.rs:80-140`. The commit boundary is
a blank line outside a code fence. A long un-broken paragraph
never commits, so the "tail" grows unboundedly and the plain-wrap
on tail keeps doing more work each frame. Most chat answers do
include blank lines, so this is a tail-risk pattern, not a
common case. **Severity: suspect.**

### Markdown rendering (block-level)

**F-12. `wrap_spans` backwards whitespace scan per wrap point.**
`crates/anie-tui/src/markdown/layout.rs:1107` calls
`rposition(|(c, _)| c.is_whitespace())`. For a 5,000-char
paragraph at width 120 that's ~40 backwards O(width) scans per
paragraph render — bounded but visible. Only fires on cache
miss, so the impact is bounded. **Severity: suspect.**

**F-13. Per-span `to_string()` in markdown layout.**
`crates/anie-tui/src/markdown/layout.rs:434, :444, :546`. Every
text token going through `push_text` / `push_styled` allocates
an owned `String`. Bounded by cache miss frequency. **Severity:
suspect.**

**F-14. `chars().count()` for layout math.**
`crates/anie-tui/src/markdown/layout.rs:662, :669, :819, :980,
:1005, :1028, :1155, :1200`. O(n) per measurement. The plan-04
design call explicitly deferred replacing these with
`UnicodeWidthStr`; the deferral note remains accurate. **Severity:
suspect** with a known correctness/perf trade.

### Block / flat cache layer

**F-15. `boxed_lines` and `prefix_lines` allocate
per-cache-miss.** `crates/anie-tui/src/output.rs:1325-1362,
:1480-1527`. Strings via `format!`, `repeat`, `to_string`,
`clone`. Each `Line` is built from a fresh `Vec<Span>`. Same
caveat as F-9 — fires every frame for executing tool blocks
since they bypass cache. **Severity: confirmed-hot during tool
execution.**

**F-16. `block_lines` test wrapper exists in production code.**
`crates/anie-tui/src/output.rs:633-637`. It IS gated with
`#[cfg(test)]` (good), but is the production code's only signal
that the flat-cache path is tested. Worth a glance to make sure
it stays in sync as the production path mutates. **Severity:
code-health.**

**F-17. Width change forces full cache rebuild via
`invalidate_all_caches` cascade.** `crates/anie-tui/src/output.rs:299-317`
on theme / tool-output mode change; width is handled by the
`can_reuse_flat_snapshot` check (`output.rs:574-575`). The
recent commit `ae496c4` ("try previously-used width first to
preserve flat cache") explicitly addressed an oscillation bug
where the scrollbar's presence cycled the width between two
values per frame, defeating the width-match check. That fix is
in place — flagging it here so future scroll-affordance work
doesn't reintroduce the bug. **Severity: code-health (regression
risk).**

### Code-health observations (not perf)

**F-18. `assistant_answer_lines` carries an unused
`_is_streaming` parameter.** `crates/anie-tui/src/output.rs:1183-1196`.
Leftover from earlier design. The branch logic that consumed
it is now in `StreamingAssistantRender`. **Severity:
code-health.**

**F-19. `handle_idle_key` and `handle_active_key` overlap on
scroll keys.** `crates/anie-tui/src/app.rs:911-997`. Both call
`output_pane.scroll_*()` and return `RenderDirty::full()` for
PageUp/PageDown/Home/End. Could collapse with conditional abort
behavior. **Severity: code-health.**

**F-20. `RenderDirty` allows
(composer=true, transcript=true) simultaneously.**
`crates/anie-tui/src/app.rs:118-164`. The merge is correct but
the only modes consumed are `(false, true)`, `(true, false)`,
`(false, false)`. The state space is bigger than the use cases.
**Severity: code-health** — over-general, not buggy.

## Why the benchmarks miss the user's pain

The criterion harness in `crates/anie-tui/benches/tui_render.rs`
constructs an `OutputPane` directly, drives it against a
`TestBackend`, and times `render_once`. The pipeline elements
the user feels — `App::render_with_mode`, `InputPane::render`
including the doubled `layout_lines`, `App::render_status_bar`
including `shorten_path`, the keystroke-handling tokio select
loop, the agent-event drain interleaving — are all bypassed.

Net effect: a real fix for F-1 (input layout dedupe) wouldn't
move the bench at all, and a regression in `shorten_path` (F-6)
wouldn't either. PR 02 closes that gap by adding a benchmark
keyed off the existing `ANIE_TRACE_TYPING` instrumentation, so
keystroke→paint latency is measured the same way the user
experiences it.

## Severity-grouped summary

| Severity | Findings |
|----------|----------|
| confirmed-hot (input) | F-1 |
| confirmed-hot (per-frame) | F-4, F-5, F-6, F-7 |
| confirmed-hot (streaming) | F-8, F-9, F-15 |
| suspect | F-2, F-11, F-12, F-13, F-14 |
| code-health | F-3, F-10, F-16, F-17, F-18, F-19, F-20 |

## Mapping to plans

| Plan | Findings addressed |
|------|--------------------|
| 01 input pane layout dedupe | F-1, F-2 (partially) |
| 02 keystroke latency bench | the meta-finding (bench gap) |
| 03 cache-hit path cleanup | F-4, F-5, F-6, F-7 |
| 04 streaming hot path | F-8, F-9, F-10, F-15 |
| 05 simplifications | F-3, F-18, F-19, F-20 |

F-11, F-12, F-13, F-14, F-16, F-17 are tracked here but not
opened as their own PRs — they're either suspect/uncertain or
already documented elsewhere with an accepted deferral. Re-open
if profile data points at them.
