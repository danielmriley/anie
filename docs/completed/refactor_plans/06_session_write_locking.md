# Plan 06 — Session write locking

## Motivation

`crates/anie-session/src/lib.rs` opens the session file in append
mode and calls `flush()` after every write. It assumes a single
writer per session file but does not enforce this. Concrete failure
modes:

- Two `anie` processes both running `--resume <same-id>` interleave
  appends. Partial records can appear mid-line.
- `parse_session_file()` skips malformed JSON lines with a warning
  and continues. Silent data loss on the skipped line.
- A process crash mid-write leaves a partial JSON line; the next
  read drops that entry.

The recovery behavior is already good (graceful skip + warn). What's
missing is prevention.

## Design principles

1. **Lock on open, release on drop.** Native advisory file locking
   via `fd-lock` or equivalent. Other writers get a clear error, not
   corruption.
2. **Fall back cleanly on platforms that don't support it.** Log a
   warning if the lock can't be acquired; proceed as today. Do not
   fail hard on unsupported filesystems (e.g., some network
   filesystems lack locking).
3. **Atomic writes where it's cheap.** For entries smaller than a
   page, a single `write_all` + `flush` on a `File` in append mode
   is already atomic enough on Linux/macOS. We do not need
   write-and-rename.
4. **Document the assumption.** The README and `ARCHITECTURE.md`
   should say "single writer per session file." The lock enforces
   it; the docs explain it.

## Alternatives considered

- **Write-and-rename per entry:** too much syscall overhead (a
  rename per append) and doesn't solve concurrent writers anyway.
- **Sqlite-backed sessions:** tracked in `docs/ideas.md` as a longer
  conversation. Not this plan.
- **CRDT / append log per writer:** overkill. Users want "I resumed
  twice by accident; recover gracefully."

---

## Phase 1 — Add `fd-lock` (or equivalent) dependency

**Goal:** Pick a cross-platform advisory-lock crate and wire it in.

### Files to change

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Add `fd-lock = "4"` to `[workspace.dependencies]` |
| `crates/anie-session/Cargo.toml` | Add `fd-lock.workspace = true` |

### Sub-step A — Crate choice

`fd-lock` is the usual pick: works on Linux/macOS/Windows, advisory
locking, small API. Alternative: `fs4`. Either is fine; pick
`fd-lock` unless there's a project-wide preference.

### Exit criteria

- [ ] Workspace builds with the new dependency.
- [ ] No other crate inadvertently picks it up.

---

## Phase 2 — Acquire a write lock on session open

**Goal:** `Session::open` acquires an exclusive advisory lock on
the JSONL file for the lifetime of the `Session`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | Wrap the session's `File` in `fd_lock::RwLock<File>`; acquire exclusive lock on open; release via drop |

### Sub-step A — Lock acquisition

Inside `Session::open`:

```rust
use fd_lock::RwLock;

let file = OpenOptions::new()
    .create(true)
    .append(true)
    .read(true)
    .open(&path)?;

let mut locked = RwLock::new(file);
let guard = match locked.try_write() {
    Ok(g) => g,
    Err(_) => {
        return Err(SessionError::AlreadyOpen(path.clone()));
    }
};
```

Store both the `RwLock` and the guard on the `Session` struct (or
the guard lifetime tied to the `Session`).

### Sub-step B — New error variant

Add `SessionError::AlreadyOpen(PathBuf)` to the error type. Update
call sites in `anie-cli` / `anie-controller` to surface this cleanly:

> "Session <id> is already open in another process. Close it or use
>  `/fork` to branch."

### Sub-step C — Fall back on unsupported systems

If the platform returns `ENOTSUP` or equivalent, log a warning at
`warn!` level and continue without the lock. Do **not** fail — some
users run on NFS / WSL / restricted filesystems where advisory locks
are no-ops.

```rust
match locked.try_write() {
    Ok(g) => Some(g),
    Err(err) if is_lock_unsupported(&err) => {
        tracing::warn!(
            path = %path.display(),
            "filesystem does not support file locking; \
             concurrent writers will not be detected"
        );
        None
    }
    Err(_) => return Err(SessionError::AlreadyOpen(path)),
}
```

### Files that must NOT change

- `crates/anie-protocol/*` — no format change.
- `crates/anie-cli/src/controller.rs` — behavior is transparent
  unless `AlreadyOpen` surfaces.

### Test plan

| # | Test |
|---|------|
| 1 | `single_open_succeeds` |
| 2 | `second_open_same_file_fails_with_already_open` (use `tempfile::NamedTempFile` + two `Session::open` calls in one process) |
| 3 | `second_open_after_first_dropped_succeeds` (drops first, opens second; must work) |
| 4 | `write_then_reopen_sees_all_entries` (drops session, reopens, confirms parse) |
| 5 | `unsupported_lock_does_not_fail_open` (mocked via a feature flag or conditional compile if possible; else document as untested on NFS) |

### Exit criteria

- [ ] Second concurrent open returns `SessionError::AlreadyOpen`.
- [ ] Dropping the first session releases the lock; third open
      works.
- [ ] Unsupported filesystems get a warn log, not a hard error.

---

## Phase 3 — Surface `AlreadyOpen` helpfully in the CLI

**Goal:** When a user tries to resume a session that's already open,
they get a clear message and an actionable suggestion.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Catch `SessionError::AlreadyOpen`; print a helpful message; exit cleanly |
| `crates/anie-cli/src/lib.rs` | If any argument-parse layer needs to translate the error, do it here |

### Sub-step A — Message text

> "Session <id> is already open in another anie process.
>
>  Options:
>  - Close the other anie session and try again.
>  - Fork this session into a new branch: `anie --resume <id>
>    --fork`.
>  - Start a new session (omit `--resume`)."

(If `--resume --fork` is not yet a CLI flag, tailor the text. If
there's a TUI equivalent, mention it.)

### Sub-step B — Exit code

Return a non-zero exit code from `run_interactive_mode`,
`run_print_mode`, and `run_rpc_mode` in this case. Scripts invoking
`anie` deserve to notice.

### Test plan

| # | Test |
|---|------|
| 1 | `cli_exits_nonzero_on_already_open` (integration test spawning two processes against a tempdir session) |
| 2 | `cli_message_contains_fork_suggestion` |

### Exit criteria

- [ ] User gets a clear message.
- [ ] Process exits non-zero.
- [ ] No corrupted JSONL in the test artifacts.

---

## Phase 4 — Document the assumption

**Goal:** The README, architecture doc, and session module docs all
say "single writer; use `/fork` if you need a second."

### Files to change

| File | Change |
|------|--------|
| `README.md` | Add a sentence under "Sessions and runtime files" |
| `docs/arch/anie-rs_architecture.md` | Add a note under the session-persistence block |
| `crates/anie-session/src/lib.rs` | Module-level doc comment describing the lock |

### Sub-step A — README wording

Near the bullet for `~/.anie/sessions/*.jsonl`:

> anie locks the session file while it's open. If you try to
> `--resume` a session that another anie process already has open,
> the second process exits with an error instead of writing into
> the same file. Use `/fork` to branch.

### Sub-step B — Module docs

Top of `anie-session/src/lib.rs`:

```rust
//! Session persistence.
//!
//! ## Concurrency
//!
//! A session file is opened with an exclusive advisory file lock
//! (via `fd-lock`). A second attempt to open the same file returns
//! `SessionError::AlreadyOpen`. On platforms that don't support
//! advisory locks (some network filesystems), the lock attempt is
//! a no-op and a warning is logged.
//!
//! Within a single process, a `Session` owns its file; there is no
//! cross-task sharing. Concurrent writes from multiple tasks in the
//! same process are also undefined — clone the session via
//! `fork_to_child_session` if you need a second writer.
```

### Exit criteria

- [ ] README has the new bullet.
- [ ] Architecture doc mentions it.
- [ ] Module docs explain the lock behavior.

---

## Files that must NOT change in any phase

- `crates/anie-protocol/*` — JSONL format is stable.
- Tool crates.
- Config / auth crates.

## Dependency graph

```
Phase 1 (dep) ──► Phase 2 (lock on open) ──► Phase 3 (CLI msg) ──► Phase 4 (docs)
```

Strictly sequential; each phase is small.

## Out of scope

- Sqlite-backed sessions (tracked in `docs/ideas.md`).
- Write-and-rename atomicity (not needed at current append sizes).
- Cross-machine locking (out of scope; users on shared filesystems
  get the fallback-warn path).
- CRDT-style merge for concurrently-written sessions.
