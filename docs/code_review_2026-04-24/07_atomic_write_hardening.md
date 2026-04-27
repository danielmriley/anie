# 07 — Atomic write hardening

## Rationale

`atomic_write` is careful on Unix: it writes a temporary file, fsyncs,
renames, and uses a PID-based temp name. The review still found two
hardening gaps:

- Windows replacement semantics differ from POSIX `rename` over an
  existing file.
- The temp name includes the process ID but not a nonce/counter, so two
  concurrent writes to the same target in the same process can collide.

Codex uses `NamedTempFile::new_in(parent)` for unique temp names. Anie's
current fsync/rename discipline is good; borrow the uniqueness idea
without giving up explicit durability behavior.

## Design

Split the work into two parts:

1. Make temp file names collision-resistant within the same process.
2. Add a platform-specific replacement path for Windows, or explicitly
   gate Windows write support if the project chooses not to support it
   yet.

For temp uniqueness, prefer a random or monotonic nonce over thread ID
alone:

- random suffix from `rand` if already available in the workspace, or
- static `AtomicU64` counter combined with process ID.

For Windows replacement, evaluate the right Rust approach:

- `std::fs::rename` may fail if the destination exists.
- `ReplaceFileW` provides replace semantics but requires Windows API
  bindings and careful error handling.

Do not change the public behavior of auth/config writes except to make
them more reliable.

## Files to touch

- `crates/anie-config/src/lib.rs`
  - Update `atomic_write` temp naming.
  - Add Windows replacement branch or explicit tests documenting current
    unsupported behavior.
- `crates/anie-auth/src/store.rs`
  - Audit whether auth writes reuse config atomic write or a parallel
    helper.
- `Cargo.toml`
  - Only if a new dependency is genuinely needed; prefer existing deps.

## Phased PRs

### PR A — Same-process temp-name uniqueness

**Change:**

- Add a nonce/counter to temp file names.
- Preserve file mode and fsync behavior.
- Ensure cleanup of failed temp files still works best-effort.

**Tests:**

- Concurrent same-process writes to the same target do not collide on
  temp file names.
- Replacing an existing file still succeeds on Unix.

**Exit criteria:**

- PID-only temp naming is gone.

### PR B — Windows replacement semantics

**Change:**

- Add `#[cfg(windows)]` replacement implementation with proper
  overwrite semantics, or explicitly skip/gate write tests on Windows
  with a tracked rationale.
- Update comments that currently imply Windows is outside CI if that is
  no longer true.

**Tests:**

- Windows target check compiles.
- If Windows test environment is available, replacing an existing file
  succeeds.

**Exit criteria:**

- The helper no longer has a known mismatch with the repository's
  Windows CI story.

## Test plan

- `cargo test -p anie-config`
- `cargo test -p anie-auth`
- `cargo check -p anie-config --target x86_64-pc-windows-msvc`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Risks

- Windows replacement APIs can be tricky around file permissions and
  antivirus scanners. Keep the branch small and tested.
- Do not introduce a second atomic-write helper if one shared helper can
  serve config/auth/runtime state.
- Do not weaken Unix durability while improving uniqueness.

## Exit criteria

- Atomic writes have collision-resistant temp names.
- Replacement semantics are explicit and correct for supported
  platforms.

