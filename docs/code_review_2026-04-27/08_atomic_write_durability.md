# 08 — Atomic-write durability clarification

## Rationale

`anie_config::atomic_write()` writes to a same-directory temp file,
fsyncs the file, then renames over the target:

- `crates/anie-config/src/lib.rs:318-356`

This is good replacement-atomicity behavior on POSIX, and it preserves
old contents on write failures. But after a successful rename, the
parent directory is not fsynced. On some filesystems/mount options, a
crash immediately after rename can lose the directory-entry update even
though the temp file was fsynced.

The current doc comment says a crash during the write leaves the
original path intact, which is true for the write-before-rename phase,
but the helper is not fully crash-durable across the rename without a
parent-directory sync.

## Design

Choose one of two small shapes.

### Option A — Strengthen durability on Unix

After `fs::rename(&tmp, path)` succeeds:

1. Open the parent directory.
2. Call `sync_all()` on the directory handle.
3. Return success only if directory sync succeeds, or log/return the
   sync error depending on desired semantics.

This makes the helper closer to the durable-write pattern used by
storage systems.

### Option B — Clarify documentation only

If directory fsync is not worth the cross-platform complexity right now,
change the doc comment to say:

- the temp file is fsynced before rename;
- replacement is atomic on POSIX;
- the helper does not currently fsync the parent directory, so it should
  not be described as fully crash-durable after rename.

Given the helper is already Windows-gated, Option A is likely tractable
for Unix and can be paired with doc clarification.

## Files to touch

- `crates/anie-config/src/lib.rs`
  - Update `atomic_write()` implementation and/or docs.
  - Add tests if implementation changes are testable.

## Phased PRs

### PR A — Parent directory sync on Unix, docs updated

**Change:**

- After successful rename, sync the parent directory on Unix.
- Keep existing cleanup-on-error behavior for temp files.
- Update doc comment to distinguish:
  - atomic replacement;
  - file content fsync;
  - directory-entry fsync.

**Tests:**

- Existing atomic-write tests continue to pass.
- Add a test that writes successfully through the new path. Directory
  fsync behavior is hard to prove in unit tests, so test only that the
  code path works on tempdirs.

**Exit criteria:**

- Implementation and docs no longer overstate durability.

### PR B — If Option A is deferred, docs-only correction

**Change:**

- Update doc comment to accurately state current guarantees.
- Add a `TODO`/plan reference for directory fsync.

**Tests:**

- No behavior tests required beyond existing test suite.

**Exit criteria:**

- Readers no longer assume full crash durability after rename.

## Test plan

- `cargo test -p anie-config atomic_write`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Risks

- Directory fsync is platform-specific. Windows is already compile-gated
  for this crate, but macOS/Linux behavior should still be checked.
- Returning an error after rename succeeds but directory fsync fails is
  awkward: the target may already contain new data. Document semantics
  carefully if that path is possible.

## Exit criteria

- `atomic_write()` documentation matches its real guarantees.
- If implemented, parent directory sync runs after successful rename on
  supported platforms.

## Deferred

- Windows `ReplaceFileW` support. That remains outside this plan and is
  already called out by the crate-level Windows gate.
