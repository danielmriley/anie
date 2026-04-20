# Plan 14 — Persistence safety: atomic writes + auth-store discipline

Two merge-blockers from the followup review, both about how we
write user data to disk. Neither is "code the user sees" — both
are silent data-loss paths that only manifest under crash, power
loss, or corruption scenarios.

## Motivation

### Blocker 3: auth-store silently discards corrupted data

`crates/anie-auth/src/lib.rs:save_api_key_at` reads the existing
store, adds the new key, and writes the result back:

```rust
let mut store = load_auth_store_at(path).unwrap_or_default();
```

`unwrap_or_default()` turns a parse error into an empty store. If
`auth.json` got corrupted somehow (partial write, manual edit, disk
bitrot), the next `save_api_key_at` call:

1. Parses existing file → fails → returns `Default::default()` —
   an empty store.
2. Inserts the new key into the empty store.
3. Writes the now-single-entry store back to disk, **overwriting
   every other credential that used to be there**.

Credentials are the most sensitive user state we handle. Silently
losing them on a parse error is the wrong trade-off.

### Blocker 4: config / auth writes are not atomic

`fs::write(path, contents)` performs a truncate + write. If the
process crashes, the power cuts out, or the OS is killed mid-write,
the file is left truncated or partially written. On next startup:

- **Config loss** — `config.toml` may be readable but missing
  providers, or unreadable entirely.
- **Auth loss** — `auth.json` may be truncated, triggering the
  blocker 3 data-loss pattern on the next save.
- **Runtime-state loss** — `state.json` is smaller and less
  critical, but still a surprise for the user ("why did the CLI
  forget which session I was on?").

Production file-writers use the atomic-rename pattern: write to
`{path}.tmp.{pid}`, fsync, rename over `{path}`. `rename` on POSIX
is atomic for same-filesystem moves.

Production call sites found:

- `crates/anie-config/src/lib.rs:288, 597, 602`
- `crates/anie-config/src/mutation.rs:123, 238, 287`
- `crates/anie-auth/src/lib.rs:160`
- `crates/anie-auth/src/store.rs:377`
- `crates/anie-cli/src/onboarding.rs:125`
- `crates/anie-cli/src/runtime_state.rs:65`

Test-only `fs::write` call sites are not in scope.

## Scope

Two phases. Phase A is the utility + mechanical migration of every
writer. Phase B fixes the auth-specific data-loss path, building
on Phase A's atomic primitive.

No changes to providers, TUI, agent loop, or session storage. The
session writer already uses `fd-lock` and append-only semantics
(plan 06); it is not in scope for this plan.

## Design principles

1. **One utility, all callers migrate.** The atomic writer is a
   10-line helper — the value is in consistent use, not in the
   helper itself.
2. **Fail loudly on corruption.** A parse error on an existing
   credential store is never overwritten silently. The user sees
   it.
3. **Preserve recovery affordances.** When we refuse to write over
   a corrupted file, we leave both the corrupted original and a
   named copy in place so the user can inspect or hand-edit.
4. **Don't break existing semantics.** Healthy-path writes continue
   to succeed without behavior change. Atomic writes are
   indistinguishable from `fs::write` on the happy path.
5. **Same-filesystem guarantee.** Temp file lives in the **same
   directory** as the target, so `rename` is atomic. Never write
   temp to `/tmp` then try to cross-device rename.

---

## Phase A — Atomic write utility + migration

**Goal:** Every user-facing persistent file is written via a helper
that either fully succeeds or leaves the previous contents intact.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-config/src/lib.rs` | Add `pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()>`. Write to `{path}.tmp.{pid}`, fsync, rename. Use where `config.toml` is currently written. |
| `crates/anie-config/src/mutation.rs` | Replace three `fs::write` sites with `atomic_write`. |
| `crates/anie-auth/src/lib.rs` | Replace `fs::write` in auth-file write path. |
| `crates/anie-auth/src/store.rs` | Replace `fs::write` in write_store_to_path. |
| `crates/anie-cli/src/onboarding.rs` | Replace `fs::write` in provider-list persistence. |
| `crates/anie-cli/src/runtime_state.rs` | Replace `fs::write` in runtime-state persistence. |
| `crates/anie-config/src/lib.rs` (tests) | Tests for atomic_write: success, failure leaves original intact, temp cleanup on error. |

### Sub-step A — The utility

```rust
/// Write `contents` to `path` atomically: a crash mid-write
/// leaves the existing `path` intact. Uses a same-directory temp
/// file + rename so the rename is atomic on POSIX.
///
/// If the write fails, the temp file is best-effort removed.
/// The caller should treat this function's failure as "nothing
/// happened" — the previous file contents are preserved.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    // PID in the name so concurrent processes writing the same
    // file don't collide on the temp. Concurrent writers of the
    // same file are still unsafe at the logical level; the PID
    // just prevents temp-name clashes.
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let mut tmp = parent.to_path_buf();
    tmp.push(format!(
        ".{}.tmp.{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));

    // Scope the file handle so the OS sees the write fully
    // closed before rename.
    let write_result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok::<_, io::Error>(())
    })();

    if let Err(err) = write_result {
        // Best-effort cleanup. Don't surface the cleanup error.
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}
```

### Sub-step B — Mechanical migration

Each of the seven production call sites follows the same pattern:

```rust
fs::write(&path, contents).with_context(...)?
// →
anie_config::atomic_write(&path, contents.as_bytes()).with_context(...)?
```

Callers outside of `anie-config` need to import the symbol. Crates
without a direct dep on `anie-config` (none of the seven — all
already depend on it) would need to add one. Verified: every
migration target is already in a crate that imports
`anie-config`.

For `anie-auth/src/store.rs`, the write already runs with the
process holding an advisory lock via `fd_lock`. Atomicity layered
on top of locking is exactly what's needed for credentials.

### Sub-step C — Posix-only, no Windows guard needed today

anie is unix-targeted today (no Windows in CI, no Windows binaries
shipped). `rename` has slightly different semantics on Windows for
existing targets; if we ever add Windows support, this utility
should grow a `cfg(windows)` branch using `ReplaceFileW`. Leave a
comment at the definition noting this.

### Test plan

| # | Test |
|---|---|
| 1 | `atomic_write_creates_file_with_contents` — Happy path. |
| 2 | `atomic_write_replaces_existing_file_atomically` — Pre-populate, rewrite, verify contents. |
| 3 | `atomic_write_failure_preserves_original` — Provoke a write failure (invalid path parent, or a read-only directory) and verify the original file is unchanged and no stray temp file is left. |
| 4 | `atomic_write_temp_name_unique_per_pid` — String check only; verify the temp path contains the PID so concurrent processes don't clash. |
| 5 | Existing config/auth tests that round-trip writes still pass unchanged. |

### Exit criteria

- [ ] `grep -rn 'fs::write' crates/ | grep -v tests | grep -v '#\[test\]'`
      returns zero matches in the seven production sites above.
- [ ] `atomic_write` is the single writer for user-facing config
      and credential files.
- [ ] Tests 1–4 pass.
- [ ] Manual: `strace`-level verification optional — pre-populate
      a file, kill the process mid-write of a larger replacement
      via a debugger breakpoint in tests, verify the original is
      still readable.

---

## Phase B — Auth-store parse-error discipline

**Goal:** `save_api_key_at` never overwrites a store it could not
parse. Corrupted credential files produce an explicit error and
the user retains a path back to their data.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-auth/src/lib.rs` | In `save_api_key_at`, replace `load_auth_store_at(path).unwrap_or_default()` with strict handling: parse success → proceed; parse error → back up the corrupted file to a timestamped copy and return an error. |
| `crates/anie-auth/src/store.rs` | Possibly mirror in any other site that reads-then-writes the store; audit as part of this phase. |
| `crates/anie-auth/src/lib.rs` (tests) | Regression: corrupt auth.json → save_api_key_at returns Err, original (corrupted) file preserved at backup path, target file unchanged. |

### Sub-step A — The corrupted-store back-up path

On parse failure:

1. Compute `auth.json.corrupt.{YYYY-MM-DDTHH:MM:SSZ}` — a sibling
   file with an unambiguous timestamp.
2. **Copy** (not rename) the corrupt file to that path. Copy so if
   the subsequent write somehow wipes the original, the backup
   still exists.
3. Return `Err` with a message pointing the user at both files and
   telling them to re-run `/onboard` or manually edit
   `auth.json`.

A dedicated error variant makes the classifier easy. We don't have
`AnieAuthError` yet; use `anyhow::Error` with a `.context(...)`
pointing at the backup path. If a structured error type becomes
needed (for the onboarding flow to offer a "recover" button), add
it in a follow-up.

### Sub-step B — Invariant documented on the call site

At the function's doc comment, state explicitly: "this function
never overwrites a store whose existing contents could not be
parsed; callers that hit a parse error receive an `Err` with a
pointer to the quarantined file."

### Sub-step C — Bonus audit

The auth store has several read-modify-write paths. Grep for
`load_auth_store_at` — any call that subsequently writes needs
the same discipline. If none do, note that and move on.

### Test plan

| # | Test |
|---|---|
| 1 | `save_api_key_with_corrupt_store_preserves_existing_file` — Write invalid JSON to auth.json, call save_api_key_at, assert: function returns Err, original file bytes are unchanged, backup file exists at the expected sibling path. |
| 2 | `save_api_key_with_empty_file_still_fails_loud` — Zero-byte auth.json is corrupt too; same behavior. |
| 3 | `save_api_key_with_valid_store_still_succeeds` — Happy path regression. |
| 4 | `save_api_key_creates_new_store_when_file_absent` — Missing file is *not* corruption; creating a new store is correct. |

### Exit criteria

- [ ] `unwrap_or_default()` on `load_auth_store_at` results is
      removed from all write paths.
- [ ] Corrupted stores produce Err + backup file. Tests 1–4 pass.
- [ ] Missing files still create a fresh store (backward compat).
- [ ] A future caller that *wants* lenient behavior (e.g., a
      read-only "what credentials do I have?" path) can still call
      `load_auth_store_at(...).unwrap_or_default()` explicitly —
      that's allowed, just not on the save path.

---

## Phase ordering

A → B. Phase A's atomic-write utility is what Phase B's backup
step uses to write the backup (or at least fsync the quarantine).
Phase A also makes Phase B's test easier: the "write didn't
happen" assertion is now cleanly decidable.

## Risks

1. **Cross-device renames.** If the user has `~/.anie/` on a
   different filesystem from `/tmp` (shouldn't happen — we write
   the temp in the same dir), `rename` fails with `EXDEV`. The
   utility explicitly writes temp in the target's parent
   directory for exactly this reason. Test 2 covers the replace
   case.
2. **fsync cost.** `sync_all` per write adds disk-bound latency
   that `fs::write` skips. For `config.toml` (once at startup /
   rare mutations) and `auth.json` (only on credential changes),
   this is fine. For `runtime_state.json` (written after every
   model/session change), it could add ~10ms per call. If that
   becomes visible, relax to `sync_data` or accept the latency —
   the durability is worth it for user-facing state.
3. **fd-lock + atomic rename interaction.** `anie-auth/src/store.rs`
   holds an advisory lock on the auth file during read/write. When
   we rename over the locked path, the lock is released on the
   old inode but the new file has no lock until re-opened. Verify
   the locking code re-acquires the lock if needed; document the
   boundary. Acceptable because the write itself completes before
   rename.
4. **Backup files accumulate.** Phase B's backups have timestamps
   so they don't collide, but they also never get cleaned up. If
   a user has repeated corruption events, `~/.anie/` grows. Not
   blocking for merge — a `--prune-backups` command can land later
   if needed.

## Out of scope

- Session file writes (already use `fd-lock` + append-only; plan
  06 landed this).
- Migrating tests that use `fs::write` for fixture setup.
- Windows support for the atomic writer (add later).
- Structured `AnieAuthError` type (only needed if the onboarding
  UI wants to branch on error kind — current error-as-anyhow is
  enough for the user-visible message).
- Removing the silent `unwrap_or_default()` from *read-only*
  paths that don't subsequently write. Those are a separate
  readability concern, not a data-loss risk.

## Preconditions for merge

This plan plus plan 13 unblocks the four items called out in the
latest project-status review. After both land:

- Ctrl+C / Quit are responsive even during retry backoff.
- User actions (submit, quit, etc.) are never silently dropped.
- Corrupted auth.json produces a loud error and preserves data.
- Config, auth, and runtime-state files are crash-safe.

The remaining review items (discovery cache bypass, TUI overlay
business-logic, EditTool fuzzy fallback, stream debouncing,
session scaling) all land post-merge as separate plans.
