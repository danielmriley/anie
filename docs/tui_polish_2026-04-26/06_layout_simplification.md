# 06 — Layout simplification: catch up to pi's LOC count

## Rationale

Finding F-9. `crates/anie-tui/src/markdown/layout.rs` is 1,866
LOC. Pi's `packages/tui/src/components/markdown.ts` is 852 LOC
for the same visible feature set: headings, bold/italic,
inline code, fenced code blocks with syntax highlighting,
ordered + nested lists, blockquotes, tables, links.

The 3× ratio is the kind of over-engineering CLAUDE.md flags:

> "Match pi's shape unless there's a documented reason not to.
> Over-engineering past pi's shape is the single most common
> mistake caught in review passes on this project."

Concrete contributors to the gap, in rough order of impact:

1. **Tail-as-plain streaming machinery.** `committed_text`,
   `tail_text`, `cached_committed_*`, `find_safe_markdown_boundary`,
   `cached_committed_lines`. ~150 LOC. Becomes dead code once
   PR 01 lands.
2. **Table layout with explicit column negotiation.** Anie has
   a multi-pass table layout (measure cells, negotiate widths,
   wrap, render). Pi walks the table tokens emitting styled
   lines with simpler width logic. ~300 LOC potential
   reduction.
3. **List-state machine.** Tracks bullet style by depth,
   ordered numbering, nested indents. Some of this is needed;
   parts duplicate state pulldown-cmark already gives us via
   the event stream. ~80 LOC potential reduction.
4. **Helper passes that wrap a single match arm.** Several
   `fn push_*` and `fn flush_*` helpers exist for one-line
   bodies. Inline where appropriate. ~50 LOC.
5. **Code-block metadata extraction.** `info_string` parsing,
   language detection, attribute stripping — three separate
   helpers. Pi handles in one match arm. ~30 LOC.

Total target: 1,866 → ~1,200 LOC, same feature set.

## Design

This PR is a **sweep**, not a feature. It comes after PR 01
because PR 01 makes ~150 LOC structurally dead. Doing the
sweep before would either (a) leave a doomed branch alive
during the rewrite, or (b) require coordinating two
overlapping commits.

### Phases inside this PR

**Phase 1: delete dead code from PR 01.**
- Remove `StreamingAssistantRender` fields that are no longer
  read after PR 01: `committed_text`, `tail_text`,
  `cached_committed_*` (everything except the new full-render
  cache).
- Remove the `find_last_safe_markdown_boundary` helper if it
  exists.
- Remove any helper functions in layout.rs that only the
  tail-as-plain branch called.

**Phase 2: simplify table layout.**

Pi's table render
(`packages/tui/src/components/markdown.ts:679-833`) walks
table tokens emitting:
1. A header row with cell content + `│` separators.
2. A separator row (`├` / `┤` / `─`).
3. Body rows.

Cell width is computed from the longest content cell in each
column, then wrapping inside cells happens with a simple
greedy split. No multi-pass negotiation.

Anie's current table layout has more sophisticated wrapping
(re-flowing across multiple lines per cell). The win in
fidelity is small and the LOC cost is large. Adopt pi's
simpler approach: cells truncate or wrap naively.

Acceptance: a markdown table of 4 columns × 5 rows renders
correctly at width 120. Wide columns truncate at column
boundary. No regression on existing table tests.

**Phase 3: collapse list state machine.**

Anie tracks list state via a side-stack the layout engine
maintains. pulldown-cmark already gives us depth via
`Start(List)` / `End(List)` event nesting. Replace the
side-stack with a `Vec<ListContext>` tracking only the bullet
style and ordered counter per depth.

**Phase 4: helper inline pass.**

Audit `fn push_*` and `fn flush_*` helpers. Inline ones with
single-line bodies that are only called once.

**Phase 5: code block metadata in one match arm.**

Take pulldown-cmark's `Start(CodeBlock(kind))`, extract
language from `kind`, look up syntect, render. One arm, one
helper for syntect init.

### What we keep

- pulldown-cmark integration (the parser itself).
- Theme-driven styling (`MarkdownTheme`).
- Syntax highlighting via syntect (just wrap it more simply).
- Link span emission with the `(url)` fallback (PR 04 of
  perf round depends on this).
- The block-level cache (`LineCache`, multi-width entries).
- All visible features: headings, bold/italic, code blocks,
  lists, blockquotes, tables, links.

### What we don't touch

- `crates/anie-tui/src/markdown/syntax.rs` (249 LOC) — already
  thin.
- `crates/anie-tui/src/markdown/theme.rs` (71 LOC) — small.
- `crates/anie-tui/src/markdown/link.rs` (100 LOC) — small.
- `crates/anie-tui/src/markdown/parser.rs` (67 LOC) — wrapper
  around pulldown-cmark.
- `crates/anie-tui/src/markdown/mod.rs` (188 LOC) — mostly
  link extraction, recently audited.

The sweep is targeted at `layout.rs` only.

## Files to touch

Just `crates/anie-tui/src/markdown/layout.rs` and its inline
tests. Possibly minor tweaks to `mod.rs` if helpers moved.

## Phased PRs

This plan is itself the sweep PR. Phases above are commits
within it, landed together.

If the diff is too large for review, split:
- 06a: dead-code deletion (post-PR 01)
- 06b: table simplification
- 06c: list-state collapse + helper inline + code-block arm

## Test plan

1. **All existing markdown render tests must pass unchanged**
   (or update the asserts to match the new behavior, but only
   for tests that pinned over-fidelity table wrapping).
2. **Snapshot tests** — render representative inputs and diff
   the output; no visible regression.
3. **LOC target**: `layout.rs` under 1,300 LOC at the end of
   the PR.
4. **Bench gate**: no regression on `scroll_static_600`,
   `stream_into_static_600`, or any keystroke bench.

## Risks

- **Visual regression on edge-case markdown.** Tables with
  uneven column widths, deeply nested lists, code blocks
  with rare languages. Mitigate with broad fixture coverage.
- **Subtle behavior changes from pulldown-cmark event
  ordering**. The simplified state machine may handle some
  events differently than the current explicit one. Walk
  through pulldown-cmark's stream test fixtures.
- **Reviewer complains "we lost a feature."** Document each
  intentional simplification (e.g., "tables no longer re-wrap
  across cell boundaries; truncate at column edge instead").

## Exit criteria

- `layout.rs` ≤ ~1,300 LOC.
- All existing tests pass.
- No bench regression.
- Side-by-side render of representative markdown
  (`docs/test_fixtures/`-style golden output) matches or
  intentionally improves.
- `cargo test --workspace` green; clippy clean.

## Deferred

- Replacing pulldown-cmark with a custom parser. Pulldown-cmark
  is well-maintained and fast.
- Full visual parity with pi (different palette, different
  table style by design choice).
- Refactoring `mod.rs` link extraction. Already lean.
