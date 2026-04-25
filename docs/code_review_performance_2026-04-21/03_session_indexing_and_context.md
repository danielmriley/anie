# Plan 03 — session indexing + context construction

**Findings covered:** #11, #12, #13, #24, #25, #26, #27, #38

This is the "simplify the session core" plan. The performance wins
come mostly from deleting redundant state and borrowing more.

## Rationale

The report found a cluster of issues in `crates/anie-session/src/lib.rs`
that all point the same way:

- `id_set: HashSet<String>` duplicates `by_id` (**#25, #38**)
- branch traversal allocates owned `String`s unnecessarily (**#11**)
- `find_cut_point` returns a cloned `keep` vector the caller drops (**#12**)
- `list_sessions` fully deserializes entries just to summarize a file (**#13**)
- several small helper paths clone when a borrow or direct build
  would do (**#24, #26, #27**)

pi's session manager reinforces this direction: it uses a single ID
map for lookup and ID generation
(`pi/packages/coding-agent/src/core/session-manager.ts:206-212`,
`315-352`) and does path-walk-first context construction.

## Design

### 1. Make `by_id` the only membership index

Delete `id_set` from `SessionFile`. Replace its uses with
`by_id.contains_key(...)`.

For `generate_unique_id`, do **not** hard-wire the helper to
`HashMap<String, usize>` if that makes testing awkward. Prefer one
of:

```rust
fn generate_unique_id(exists: impl Fn(&str) -> bool) -> String
```

or:

```rust
fn generate_unique_id(by_id: &HashMap<String, usize>) -> String
```

The closure-based API is slightly cleaner and decouples the helper.

### 2. Borrow during branch walks

`get_branch` only needs temporary parent IDs while walking. Use
`Option<&str>` or direct `&String`/`&str` references into entries,
not owned `String`s.

### 3. Trim `find_cut_point` to what callers actually use

`compact_internal` drops the `keep` vector immediately. Change the
return type to:

```rust
(Vec<SessionContextMessage>, String)
```

That returns only:

- `discard`
- `first_kept_entry_id`

Update the existing unit test that currently asserts on `keep.len()`.

### 4. Add a lightweight summary parser for `list_sessions`

`list_sessions` should not deserialize full `SessionEntry` values
just to count messages and extract a title. Add a local peek type
that only reads:

- entry discriminant
- the minimum content needed for first-message/title extraction

This should remain a read-only helper local to `list_sessions`, not
a new public session-entry type.

### 5. Sweep the session-local cheap wins at the end

Once the structural changes land, fold in:

- `summary` / `first_kept_entry_id` clone cleanup (**#24**)
- `generate_unique_id` short-ID allocation cleanup (**#26**)
- `join_text_content` direct-buffer build (**#27**)

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | Remove `id_set`, borrow in branch walk, trim `find_cut_point`, add lightweight list parser, sweep helper cleanups. |
| session tests in `lib.rs` | Update signature-sensitive tests, add regressions. |

## Phased PRs

### PR A — remove `id_set`

1. Delete the field from `SessionFile`.
2. Update membership checks to `by_id.contains_key(...)`.
3. Add regression tests around:
   - reopening a session
   - appending entries
   - forking from an existing ID

### PR B — `open_session` / `add_entries` clone cleanup

1. With `id_set` gone, reduce the clone count in `open_session`.
2. Do the same in `add_entries`.
3. Update `generate_unique_id` to the new membership source.

### PR C — borrowed branch walk

1. Rewrite `get_branch` to avoid owned-ID allocations.

### PR D — trim `find_cut_point`

1. Change `find_cut_point` to return only `discard` +
   `first_kept_entry_id`.
2. Update the test at the current `session/lib.rs:1853` site to
   match the new return shape.

### PR E — lightweight `list_sessions`

1. Introduce a local peek struct for entry summaries.
2. Replace full `SessionEntry` parsing in `list_sessions`.
3. Add tests for:
   - normal file
   - malformed lines
   - file with large tool-result payloads

### PR F — session-local low-risk helpers

1. Fix #24, #26, and #27.
2. Keep this PR tiny and separate from the structural ones.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `open_session_rebuilds_index_without_id_set` | `anie-session/src/lib.rs` |
| 2 | `add_entries_rejects_unknown_parent_via_by_id_only` | same |
| 3 | `get_branch_returns_correct_path_without_owned_parent_tracking` | same |
| 4 | `find_cut_point_returns_first_kept_entry_id_without_keep_vec` | same |
| 5 | `list_sessions_peek_parser_skips_malformed_lines_and_extracts_title` | same |
| 6 | Existing compaction/session resume tests stay green | same / integration |

## Risks

- **Hidden `id_set` assumptions:** audit all call sites, including
  tests and helper functions, before deleting the field.
- **List-session summary drift:** the peek parser must not lose
  visible behavior around title extraction or message counts.
- **`find_cut_point` signature churn:** this intentionally touches
  tests and any helper callers; keep it in its own PR.

## Exit criteria

- [ ] `id_set` is gone.
- [ ] `get_branch` no longer allocates temporary owned parent IDs.
- [ ] `find_cut_point` no longer clones the unused `keep` vector.
- [ ] `list_sessions` no longer fully deserializes every entry just
      to summarize a session file.
- [ ] Session tests and compaction-related tests remain green.

## Deferred

- Any larger redesign of persisted session format or schema version.
- Full session-context construction refactor beyond the targeted
  cleanup listed here.
