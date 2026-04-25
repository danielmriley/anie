# Plan 07 — tool read/grep/bash/edit + truncation

**Findings covered:** #19, #20, #21, #31, #32, #33, #34, #36, #37, #56

This plan handles the tool-output paths where anie is doing repeated
allocation work or maintaining truncation logic separately in several
modules.

## Rationale

The review found:

- read-path waste in `read.rs` (**#19, #31, #32, #36, #37**)
- grep-line formatting waste in `grep.rs` (**#56**)
- bash tail rendering clone in `bash.rs` (**#20**)
- edit fuzzy normalization / BOM helpers in `edit.rs`
  (**#21, #33, #34**)

The pi comparison surfaced one useful structural idea here:

- a **shared truncation helper module** reused across read/find/grep/bash
  (`pi/packages/coding-agent/src/core/tools/truncate.ts:1-67`,
  `157-264`)

pi is **not** ahead on streaming file reads — pi still reads the whole
file and slices lines afterward
(`pi/packages/coding-agent/src/core/tools/read.ts:187-207`) — so the
read-path strategy here should follow the review, not pi.

## Design

### 1. Introduce a shared truncation helper module

Create a helper module in `anie-tools` for:

- head truncation
- tail truncation
- line truncation metadata

The goal is not to port pi literally. The goal is to stop keeping
slightly different truncation policy logic in four tools.

### 2. Clean up the read tool in stages

There are two layers of work:

1. **cheap wins now**
   - stop building `Vec<&str>` for the whole file
   - stop cloning the final output text only to measure its length
   - avoid avoidable UTF-8/image-extension helper allocations
2. **larger refactor later**
   - investigate a size-gated or streamed read path for large files

The report explicitly noted that a full streaming rewrite must handle
binary detection (`bytes.contains(&0)`) carefully. So do **not** fold
that into the small cleanups unless the behavior is fully preserved.

### 3. Make grep output build directly into the final buffer

`append_line` should:

- truncate once
- write directly into `self.output`
- avoid building intermediate `truncated_line` and `new_content`
  strings

This is a good first consumer of the shared truncation helper.

### 4. Fix the bash tail-rendering clone

`OutputCollector::render` should slice the existing tail string using
indices/char boundaries rather than cloning the full tail first.

### 5. Normalize edit text once per batch

`fuzzy_find_all_occurrences` should not normalize the full content on
every fuzzy edit. Precompute normalized content + index map once per
edit batch and reuse it.

After that, fold in the low-risk BOM / line-ending helper cleanups.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tools/src/truncation.rs` (new) or `shared.rs` | shared truncation helpers |
| `crates/anie-tools/src/read.rs` | staged read-path cleanup |
| `crates/anie-tools/src/grep.rs` | direct-buffer append path + shared truncation |
| `crates/anie-tools/src/find.rs` / `ls.rs` | adopt shared truncation metadata if useful |
| `crates/anie-tools/src/bash.rs` | tail render without full clone |
| `crates/anie-tools/src/edit.rs` | batch normalization + helper cleanups |

## Phased PRs

### PR A — shared truncation helper scaffold

1. Introduce a shared helper module.
2. Keep output notices/details shape unchanged where possible.

### PR B — grep direct-write path

1. Rewrite `append_line` to write into `self.output`.
2. Remove the two-temporary-string pattern.
3. Use the shared truncation helper for line capping if it fits.

### PR C — bash tail rendering cleanup

1. Fix `OutputCollector::render`.
2. Keep this separate from read/edit so behavior differences are easy
   to inspect.

### PR D — edit fuzzy normalization

1. Precompute normalized content for fuzzy edits.
2. Keep this separate from BOM/line-ending helpers.

### PR E — edit BOM / line-ending helper cleanup

1. Land the BOM / line-ending helper cleanups.
2. Keep the public edit behavior unchanged.

### PR F — read-path cheap wins (output-body work)

1. Remove the whole-file `Vec<&str>` build.
2. Remove the final `text.clone()` for JSON details.
3. Keep this PR strictly about output-body construction.

### PR G — read helper cleanup

1. Fold in image-extension / UTF-8 helper cleanups.
2. Keep this separate from the body/offset work.

### PR H — streamed / size-gated read follow-up (optional)

1. Document whether the larger-file streaming rewrite for #19 is
   landing now or being deferred.
2. If it lands, keep binary detection behavior explicitly tested.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | shared truncation helper head/tail cases | new helper tests |
| 2 | `grep_append_line_respects_existing_byte_limit_and_notice_behavior` | `grep.rs` tests |
| 3 | `read_offset_limit_behavior_is_unchanged_after_vec-removal` | `read.rs` tests |
| 4 | `read_binary_detection_still_short-circuits_before_text_render` | `read.rs` tests |
| 5 | `bash_output_collector_render_matches_existing_tail_behavior` | `bash.rs` tests |
| 6 | `edit_multiple_fuzzy_replacements_normalize_content_once` | `edit.rs` tests |

## Risks

- **Behavior drift in truncation notices:** a shared helper is good,
  but the user-visible continuation text must remain actionable.
- **Read-path binary detection:** this is the main reason to split the
  read work into cheap wins vs. larger streaming refactor.
- **Edit fuzzy matching correctness:** precomputing normalized data is
  worthwhile only if the index map remains exact.

## Exit criteria

- [ ] Tool truncation logic has a shared helper module.
- [ ] `grep` no longer allocates two temporary `String`s per emitted
      line.
- [ ] `read` no longer builds a whole-file `Vec<&str>` just to slice
      by offset/limit.
- [ ] `bash` no longer clones the tail string just to render it.
- [ ] `edit` no longer normalizes the full content once per fuzzy edit.

## Deferred

- Full streaming read implementation if the binary-detection behavior
  cannot be preserved in the same PR.
- Any expansion of the shared truncation helper beyond the four tool
  paths covered here.
