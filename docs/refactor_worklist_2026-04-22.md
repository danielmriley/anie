# anie refactor worklist — 2026-04-22

Master sequencing for the perf + polish work described across
two plan sets:

- `docs/code_review_performance_2026-04-21/` — 10 plans
  (~45 PRs) derived from the full performance code review.
- `docs/tui_perf_architecture/` — 8 plans (~20 PRs)
  specifically targeting "TUI feels sluggish" after pi/Codex
  architecture research.

This worklist doesn't duplicate the plans. It **sequences**
them: what lands first, what can run in parallel, where the
overlaps are, and what's optional. When the two plan sets
refer to the same work, the canonical reference is named
below.

## Overlap resolution

There are exactly two overlaps between the two folders:

1. **`code_review_performance_2026-04-21/04_tui_output_hot_path.md`**
   is the same work as **`tui_perf_architecture/03_land_plan_04.md`**.
   Canonical home: `code_review_performance_2026-04-21/04`.
   The `tui_perf_architecture/03` doc is an execution view
   of the same PRs.
2. **Synchronized output (BSU/ESU)** is mentioned in the
   Codex comparison as something Codex has but is not
   called out in the code-review plans. Canonical home:
   `tui_perf_architecture/02`.

Everything else is non-overlapping.

## Priority framing

User-visible sluggishness is the headline pain. The ordering
below therefore front-loads TUI-facing work even when
backend-cleanup PRs (e.g. tool registry) are individually
smaller and cheaper. The backend cleanups still ship; they
just don't block the perceived-perf improvements.

The one exception: **Phase 0 must land before anything else.**
We do not ship blind fixes again (Plan 09 viewport slicing
delivered a real but wrong-target win without a flamegraph;
we don't repeat that).

---

## Phases

### Phase 0 — Measure (blocks everything)

| # | PR | Source | Notes |
|---|----|--------|-------|
| 0.1 | tui_perf 01 PR-A | `tui_perf_architecture/01` | perf-trace JSONL + spans |
| 0.2 | tui_perf 01 PR-B | `tui_perf_architecture/01` | criterion benchmark scaffold |
| 0.3 | tui_perf 01 PR-C | `tui_perf_architecture/01` | flamegraph capture + top-5 writeup |

**Exit criteria:**
- `cargo bench -p anie-tui` runs 3 scenarios
  (`scroll_static_600`, `stream_into_static_600`,
  `resize_during_stream`) and writes numbers to
  `docs/tui_perf_architecture/execution/baseline_numbers.md`.
- `flamegraph_baseline.svg` (or referenced artifact) checked
  in or linked.
- Top-5 hot functions with file:line recorded.

No PRs below this phase cite a before/after number without
referencing this baseline.

---

### Phase 1 — Cheap, independent wins (parallel)

All three of the following can land in parallel — they touch
disjoint files and have no dependencies between each other.
Land them alongside Phase 2 preparation.

| # | PR | Source | Files | Risk |
|---|----|--------|-------|------|
| 1.1 | tui_perf 02 | `tui_perf_architecture/02` | `anie-tui/src/terminal.rs`, `anie-tui/src/app.rs` | None — BSU/ESU is a hint |
| 1.2 | code_review 01 PR-A | `code_review_performance_2026-04-21/01` | `anie-tools` registry | Low |
| 1.3 | code_review 01 PR-B | `code_review_performance_2026-04-21/01` | `anie-tools` precompiled validators | Low |
| 1.4 | tui_perf 06 PR-A | `tui_perf_architecture/06` | `anie-tui/src/input.rs` | None |

**Exit criteria:**
- `terminal.draw` calls wrapped in BSU/ESU with env-var
  escape hatch.
- Tool-definition sort + validator compile happens once, not
  per call.
- Autocomplete refresh debounced at ~80 ms.

---

### Phase 2 — TUI hot path (the big one)

Canonical: `docs/code_review_performance_2026-04-21/04_tui_output_hot_path.md`.
This is the largest individual perf win and the riskiest
TUI-side change. Budget accordingly — one PR at a time, each
with a before/after from the Phase 0 benchmark.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 2.1 | code_review 04 PR-A | Plan 04 | render scheduling cross-link |
| 2.2 | code_review 04 PR-B | Plan 04 | Arc-backed output cache (aka tui_perf 03 PR-A) |
| 2.3 | code_review 04 PR-C | Plan 04 | cache read-side + invalidation audit |
| 2.4 | code_review 04 PR-D | Plan 04 | `wrap_plain_text` rewrite |
| 2.5 | code_review 04 PR-E | Plan 04 | `wrap_spans` rewrite (aka tui_perf 03 PR-B) |
| 2.6 | code_review 04 PR-F | Plan 04 | render helper cleanup (aka tui_perf 03 PR-C) |

**Exit criteria:**
- `scroll_static_600` bench: allocations/frame drop ≥ 50%
  vs Phase 0 baseline.
- `stream_into_static_600` bench: p50 frame time drops
  ≥ 30% vs Phase 0 baseline.
- Flamegraph re-capture: no Plan 04-scope function remains
  in top-20 self-time.

The in-flight state of the tracker at
`code_review_performance_2026-04-21/execution/README.md`
and `tui_perf_architecture/execution/README.md` both update
for each PR. Source of truth on status: code-review tracker.

---

### Phase 3 — Streaming architecture (the other big one)

After Phase 2, streaming-block frame cost is the remaining
hot path. Two plans compose:

- `tui_perf_architecture/04` cuts *how often* we redraw
  during a stream (delta batching + backpressure).
- `tui_perf_architecture/08` cuts *how much work* each
  redraw does (Codex-style newline-gated commit with
  cached committed text).

Landing order matters — 04 first, then 08, so we measure
the effect of each independently.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 3.1 | tui_perf 04 PR-A | `tui_perf_architecture/04` | drain-and-batch deltas per frame |
| 3.2 | tui_perf 04 PR-B | `tui_perf_architecture/04` | bounded mpsc channel |
| 3.3 | tui_perf 08 PR-A | `tui_perf_architecture/08` | `StreamingMarkdownBuffer` + unit tests |
| 3.4 | tui_perf 08 PR-B | `tui_perf_architecture/08` | wire collector into OutputPane |
| 3.5 | tui_perf 08 PR-C | `tui_perf_architecture/08` | finalize-flush + width-change-mid-stream |

**Exit criteria:**
- `stream_into_static_600` p50 drops **another** ≥ 50%
  on top of Phase 2's numbers.
- Markdown parser allocations per second during a stream
  track newline-rate, not delta-rate (≥ 10× reduction for
  typical streams).
- Width-change-mid-stream regression test passes.
- Unclosed-fence edge case handled.

**Gate:** if Phase 3.1+3.2 already close the perceived-
sluggishness gap (subjective smoke + benchmark evidence),
consider deferring 3.3–3.5. Don't land architecture that
isn't needed.

---

### Phase 4 — Cache hardening

Edge-case correctness and long-session behavior. Not
median-case perf, but fixes specific "feels laggy" moments.
Can run in parallel with Phase 3.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 4.1 | tui_perf 05 PR-A | `tui_perf_architecture/05` | `BlockRender` merge (line + link cache) |
| 4.2 | tui_perf 05 PR-B | `tui_perf_architecture/05` | per-block link-map |
| 4.3 | tui_perf 05 PR-C | `tui_perf_architecture/05` | resize debounce + width-keyed cache |
| 4.4 | tui_perf 06 PR-B | `tui_perf_architecture/06` | stall-aware spinner |
| 4.5 | tui_perf 06 PR-C | `tui_perf_architecture/06` | mouse-motion trace / fix |

**Exit criteria:**
- Resize drag doesn't stutter.
- Stalled stream doesn't spin the CPU animating a spinner.
- `find_link_ranges` self-time ≤ 1% on scroll bench.

---

### Phase 5 — TUI features (scrolling, tables, display modes)

User-visible polish. Separate from the perf work but
touches `output.rs` and markdown code, so landing after
Phases 2–4 reduces merge conflict risk.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 5.1 | code_review 09 PR-A | Plan 09 | config + pane plumbing |
| 5.2 | code_review 09 PR-B | Plan 09 | compact rendering for `bash` / `read` |
| 5.3 | code_review 09 PR-C | Plan 09 | `/tool-output` runtime toggle |
| 5.4 | code_review 10 PR-A | Plan 10 | real transcript scrollbar |
| 5.5 | code_review 10 PR-B | Plan 10 | scrollbar mouse interaction |
| 5.6 | code_review 10 PR-C | Plan 10 | width-aware markdown tables |

**Exit criteria:**
- Verbose/compact tool-output toggle works and persists.
- Scrollbar renders and accepts mouse drag.
- Markdown tables don't break width on narrow terminals.

---

### Phase 6 — Picker + fuzzy

Keystroke-path structural improvements (query
normalization, tokenized filtering). High-frequency code
but small-surface change.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 6.1 | code_review 05 PR-A | Plan 05 | lowered-query scorer API |
| 6.2 | code_review 05 PR-B | Plan 05 | tokenized model-picker filtering |
| 6.3 | code_review 05 PR-C | Plan 05 | autocomplete lowercase caches |
| 6.4 | code_review 05 PR-D | Plan 05 | text-field helpers |

**Exit criteria:**
- Typing in a picker with 500-entry catalog feels
  instantaneous.
- No allocations in the steady-state query-and-filter path.

---

### Phase 7 — Agent + session internals

Deep backend cleanup. No user-visible output; purely
removes clone pressure and simplifies data structures. Lower
priority than the TUI work but still valuable.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 7.1 | code_review 02 PR-A | Plan 02 | `Cow` sanitization fast path |
| 7.2 | code_review 02 PR-B | Plan 02 | prompt replay ownership cleanup |
| 7.3 | code_review 02 PR-C | Plan 02 | tool-result ownership cleanup |
| 7.4 | code_review 02 PR-D | Plan 02 | `finish_with_assistant` cleanup |
| 7.5 | code_review 02 PR-E | Plan 02 | centralized run-finalization |
| 7.6 | code_review 03 PR-A | Plan 03 | remove `id_set` |
| 7.7 | code_review 03 PR-B | Plan 03 | open_session / add_entries clone cleanup |
| 7.8 | code_review 03 PR-C | Plan 03 | borrowed branch walk |
| 7.9 | code_review 03 PR-D | Plan 03 | `find_cut_point` trim |
| 7.10 | code_review 03 PR-E | Plan 03 | lightweight `list_sessions` |

**Exit criteria:**
- `cargo test --workspace` green.
- Flamegraph on agent-heavy workload: no
  `clone_from_slice` / `String::clone` in top-30.

---

### Phase 8 — Provider streaming cleanup

Correctness-sensitive work (Anthropic request ordering,
empty-delta handling). Land **after** Phases 2 and 3 so any
streaming-TUI integration bugs surface while this code is
still stable.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 8.1 | code_review 06 PR-A | Plan 06 | Anthropic request-body ordering |
| 8.2 | code_review 06 PR-B | Plan 06 | Anthropic empty-delta cleanup |
| 8.3 | code_review 06 PR-C | Plan 06 | OpenAI empty-delta cleanup |
| 8.4 | code_review 06 PR-D | Plan 06 | tagged reasoning splitter cleanup |
| 8.5 | code_review 06 PR-E | Plan 06 | model-discovery cache ownership |
| 8.6 | code_review 06 PR-F | Plan 06 | local probe normalization |

**Exit criteria:**
- Existing provider integration tests pass unchanged.
- Correctness-sensitive Anthropic ordering preserved — add
  a golden-body regression test.

---

### Phase 9 — Tool paths

| # | PR | Source | Notes |
|---|----|--------|-------|
| 9.1 | code_review 07 PR-A | Plan 07 | shared truncation helper scaffold |
| 9.2 | code_review 07 PR-B | Plan 07 | grep direct-write path |
| 9.3 | code_review 07 PR-C | Plan 07 | bash tail rendering cleanup |
| 9.4 | code_review 07 PR-D | Plan 07 | edit fuzzy normalization |
| 9.5 | code_review 07 PR-E | Plan 07 | edit BOM / line-ending helper cleanup |
| 9.6 | code_review 07 PR-F | Plan 07 | read-path cheap wins |
| 9.7 | code_review 07 PR-G | Plan 07 | read helper cleanup |

**Exit criteria:**
- Every tool produces identical output for a fixed input
  suite pre/post.
- Shared truncation helper replaces ≥ 3 duplicated
  implementations.

---

### Phase 10 — Low-risk helper sweep

Cheap wins. Lands last so hot paths have already
stabilized and the diffs stay tight.

| # | PR | Source | Notes |
|---|----|--------|-------|
| 10.1 | code_review 08 PR-A | Plan 08 | text assembly helper sweep |
| 10.2 | code_review 08 PR-B | Plan 08 | token-estimation helper cleanup |
| 10.3 | code_review 08 PR-C | Plan 08 | model-catalog cleanup |
| 10.4 | code_review 08 PR-D | Plan 08 | remaining TUI/CLI helpers |

---

### Optional / fallback / trailing

Only ship these if the earlier phases didn't cover the
underlying concern. Each individually justified; none
blocking.

| # | PR | Source | Gate |
|---|----|--------|------|
| O.1 | code_review 02 PR-F | Plan 02 | optional `AgentEnd` payload change — needs downstream buy-in |
| O.2 | code_review 03 PR-F | Plan 03 | session-local helper sweep |
| O.3 | code_review 06 PR-G | Plan 06 | provider helper sweep |
| O.4 | code_review 07 PR-H | Plan 07 | streamed / size-gated read follow-up |
| O.5 | code_review 10 PR-D | Plan 10 | horizontal overflow follow-up |
| O.6 | tui_perf 07 (whole plan) | `tui_perf_architecture/07` | pi-style line-diff — only if post-Phase-3 flamegraph shows ratatui paint as the bottleneck |

---

## Parallel tracks

When multiple contributors (or parallel agents) are
available, these tracks have no shared files and can run
concurrently without merge conflict risk:

- **Track TUI-PERF**: Phase 0 → Phase 2 → Phase 3 →
  Phase 4 → Phase 5. Sequential within; no branches.
- **Track BACKEND**: Phase 1 code_review 01 → Phase 7 →
  Phase 8 → Phase 9 → Phase 10. Largely independent of
  TUI-PERF except for light touches to `output.rs` in
  Phase 7.5 `finish_with_assistant` cleanup.
- **Track POLISH**: Phase 1 tui_perf 02, 06 PR-A → Phase 4
  PRs 4.4, 4.5 → Phase 6. Small, low-coordination PRs good
  for an opportunistic contributor.

If only one person is working through this: do TUI-PERF
start to finish, then BACKEND, then POLISH as filler.

---

## Cross-cutting risks

- **Merge conflicts on `output.rs`**. Phases 2, 3, 4, 5
  all touch it. Land Phase 2 first, let it settle one
  working day before starting Phase 3. Phase 4 (BlockRender
  merge) assumes Phase 2's Arc-wrapped cache is already in.
- **Benchmark noise**. Accept ±15% variance on p50 numbers.
  Report three runs; use median. See
  `tui_perf_architecture/01_baseline_measurement.md` for
  the full protocol.
- **Streaming regressions during Phase 3**. The Codex-style
  collector changes the shape of the streaming assistant
  block. Regression tests must include: mixed `\r\n`,
  unclosed code fences across a newline, width change
  mid-stream, and a byte-identical final-render test.
- **Correctness-sensitive Anthropic ordering** in Phase 8.
  Don't combine with other PRs. Land alone with a golden
  request-body test.

---

## Definition of done for the whole worklist

- [ ] Phase 0 baseline numbers checked in.
- [ ] Phases 1–4 landed.
- [ ] `scroll_static_600` and `stream_into_static_600`
      benchmarks show ≥ 3× p50 improvement vs Phase 0
      baseline.
- [ ] Subjective smoke on Ghostty, gnome-terminal, and
      tmux: TUI feels instantaneous for 600+ block
      transcripts with active streaming.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Every PR in phases 5–10 either landed or explicitly
      deferred with rationale in the relevant execution
      tracker.
- [ ] Optional / fallback PRs either landed or documented
      as unnecessary with flamegraph evidence.

---

## Status tracking

Two execution trackers stay live alongside their plan
folders. This worklist doesn't duplicate their state; it
cross-references them:

- `docs/code_review_performance_2026-04-21/execution/README.md`
- `docs/tui_perf_architecture/execution/README.md`

When a PR lands, update the corresponding tracker in its
home folder. Update this worklist only for structural
changes (new phase added, ordering changed, optional PR
promoted/demoted).

---

## References

- `docs/code_review_performance_2026-04-21.md` — source of
  findings (in repo root).
- `docs/code_review_performance_2026-04-21/README.md` —
  original 10-plan index.
- `docs/tui_perf_architecture/README.md` — 8-plan index
  with research synthesis.
- `docs/tui_responsiveness/README.md` — prior-phase TUI
  perf plan (already partially landed as Plan 09
  viewport slicing).
- `CLAUDE.md` — project-level conventions (plan structure,
  commit style, per-PR gate).
