# 02 — List spacing: honor pulldown-cmark's tight/loose distinction

## Rationale

Finding F-2. Anie's markdown rendering inserts a blank line for
every Paragraph end, including inside list items. Result:

```
- first item

- second item

- third item
```

…even when the source markdown has no blank lines between items
(`- first\n- second\n- third`). pulldown-cmark distinguishes
*tight* lists (no Paragraph wrappers around item content) from
*loose* lists (Paragraph-wrapped item content); anie's layout
treats them identically.

Pi and codex both honor the distinction:
- pi: `packages/tui/src/components/markdown.ts:546-611` walks
  list tokens and only emits inter-item spacing when the
  source explicitly has it.
- codex: same behavior via its own markdown renderer.

User report: "lists give a lot of space between them."

## Design

Track list nesting explicitly during pulldown-cmark traversal
in `crates/anie-tui/src/markdown/layout.rs`. Suppress
paragraph-end blank lines while inside a list item.

### pulldown-cmark behavior

For a tight list `- a\n- b`, the events are:
```
Start(List(None))
  Start(Item)
    Text("a")
  End(Item)
  Start(Item)
    Text("b")
  End(Item)
End(List(None))
```

For a loose list with `- a\n\n- b`:
```
Start(List(None))
  Start(Item)
    Start(Paragraph)
      Text("a")
    End(Paragraph)
  End(Item)
  Start(Item)
    Start(Paragraph)
      Text("b")
    End(Paragraph)
  End(Item)
End(List(None))
```

So the "loose" signal is the presence of `Paragraph` events
inside `Item`. The fix:

1. Maintain a `list_depth: usize` counter, incremented on
   `Start(List)` and decremented on `End(List)`.
2. Maintain an `in_list_item: bool` flag (or a stack tracking
   item-level Paragraph emit count) per outermost item.
3. On `Start(Paragraph)` inside a list item, record that this
   item is "loose."
4. On `End(Paragraph)` *not* inside a list item, emit the
   blank-line. On `End(Paragraph)` inside a tight item, just
   flush the line (no extra blank).

### Edge cases

- **Loose list mixed with tight items.** pulldown-cmark
  considers a list loose if *any* item has Paragraphs, but
  emits Paragraph events only for those items. Treating each
  item independently (per-item loose flag) matches both pi and
  codex behavior. Verify against fixtures.
- **Nested lists.** Inner lists should respect their own
  loose/tight classification independent of the outer.
  `list_depth` + per-item flag handles this.
- **Trailing blank line after list.** Should still render — the
  blank between the list and following content is *outside* the
  list scope, so the `End(List)` event triggers normal
  inter-block spacing.

## Files to touch

- `crates/anie-tui/src/markdown/layout.rs` — paragraph-end and
  list-item-end handlers. Probably 30–50 LOC of state-tracking
  changes; possibly 1-2 helpers.
- New tests in the existing `markdown::layout::tests` module.

## Phased PRs

Single small PR.

## Test plan

1. **`tight_list_renders_without_inter_item_blanks`** — input
   `- one\n- two\n- three`. Render. Assert output line count
   equals 3 (one per item, no separating empties).
2. **`loose_list_renders_with_inter_item_blanks`** — input
   `- one\n\n- two\n\n- three`. Render. Assert output line
   count equals 5 (three items + two blank separators).
3. **`tight_list_followed_by_paragraph_has_one_blank_separator`**
   — input `- one\n- two\n\nA paragraph.`. Render. Assert
   3 lines + 1 blank + 1 paragraph line.
4. **`nested_tight_inside_loose_renders_each_independently`**
   — input ```
- outer one
- outer two
  - inner a
  - inner b
- outer three
```
   The outer list is tight, but pulldown-cmark may make the
   middle item loose because of its sublist. Pin the expected
   line count after observing pulldown-cmark's actual behavior.
5. **Regression**: existing list tests in `markdown::layout::tests`
   continue to pass (or are updated to reflect the new behavior
   if they were asserting the old wrong-spacing).

## Risks

- **pulldown-cmark's loose-detection edge cases.** The library
  has its own rules for what counts as loose. Verify with
  fixtures rather than assuming.
- **Tests that pinned the old wrong behavior.** If any existing
  test asserts "lists have blank lines between items," it
  needs updating. Audit during implementation.
- **Visual regression on intentionally loose lists.** The fix
  should *only* change tight lists; loose lists must look
  unchanged. Test #2 guards this.

## Exit criteria

- All 4-5 tests above pass.
- `cargo test --workspace` green; clippy clean.
- Manual smoke: stream a response containing a tight bullet list
  and verify it renders compact.
- No bench regression (the fix is in cache-miss path only).

## Deferred

- Definition lists (pulldown-cmark feature, off by default).
- Task lists (`- [ ]` / `- [x]`). Already work as plain text;
  styling them is a separate polish item.
