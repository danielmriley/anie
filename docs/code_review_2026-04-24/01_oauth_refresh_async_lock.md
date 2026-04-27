# 01 — OAuth refresh lock async isolation

## Rationale

The review found that OAuth token refresh lock acquisition runs inside
an async request path but uses blocking polling:

- `crates/anie-auth/src/refresh.rs` reads credentials, then calls
  `self.acquire_lock(provider_name)?` from the async
  `resolve_access_token` flow.
- `OAuthRefresher::acquire_lock` loops on `fs4::FileExt::try_lock_exclusive`
  and waits with `std::thread::sleep(Duration::from_millis(50))` until the
  configured timeout.

The cross-process file lock itself is the right design. It protects the
shared `auth.json` when two anie processes refresh the same provider at
the same time. pi's refresh path is async and request-timeout-bounded,
but does not provide this same Rust-side file-lock structure. Codex uses
an in-process `tokio::sync::Mutex` plus reload/double-check logic, which
does not protect separate processes sharing the same auth store.

The bug is narrower: lock polling should not occupy a Tokio worker for
up to the 15 second lock timeout.

## Design

Keep the existing refresh protocol:

1. Read the credential.
2. Return immediately if it is not near expiry.
3. Acquire the provider lock.
4. Re-read credential after acquiring the lock.
5. Refresh only if the re-read credential is still near expiry.
6. Persist rotated tokens.
7. Return the access token and release the lock when the file handle is
   dropped.

Change only the blocking boundary.

Preferred implementation: move the full blocking lock acquisition into
`tokio::task::spawn_blocking`, returning the locked `std::fs::File` to
the async caller. This keeps all `fs4` behavior on blocking threads,
preserves the current timeout semantics, and avoids mixing short
blocking file-lock calls with async sleeps.

Alternative implementation: make lock acquisition async by calling
`try_lock_exclusive` once per loop and using `tokio::time::sleep`
between attempts. This is acceptable only if each `try_lock_exclusive`
call is guaranteed to be non-blocking on supported platforms.

## Files to touch

- `crates/anie-auth/src/refresh.rs`
  - Add an async lock-acquisition helper.
  - Move the blocking loop into `spawn_blocking`, or replace the sleep
    loop with async sleep.
  - Preserve `RefreshError::LockTimeout` and `RefreshError::Persist`.
- `crates/anie-auth/src/lib.rs` or auth tests
  - Add or update tests for contention and timeout behavior.

## Phased PRs

### PR A — Isolate blocking acquisition

**Change:**

- Introduce `async fn acquire_lock_async(&self, provider_name: &str)`.
- Call it from `resolve_access_token`.
- Keep the synchronous `acquire_lock` private and execute it inside
  `spawn_blocking`, or replace it entirely with an async polling helper.

**Tests:**

- A lock held by another file handle causes `resolve_access_token` to
  return `RefreshError::LockTimeout` after a short test timeout.
- A successful acquisition still follows the double-check-after-lock
  path.

**Exit criteria:**

- No `std::thread::sleep` remains reachable from async refresh logic
  outside `spawn_blocking`.
- Existing refresh tests pass unchanged except where assertions must be
  updated for async helper names.

### PR B — Cancellation and contention regression coverage

**Change:**

- Add a regression test that starts a contended refresh while unrelated
  async work continues to make progress.
- If practical, use `tokio::time::pause` / timeout-driven tests to avoid
  slow wall-clock sleeps.

**Tests:**

- Contended refresh does not block a second lightweight async task from
  completing.
- Lock timeout remains typed as `RefreshError::LockTimeout`, not a
  generic join or IO error.

**Exit criteria:**

- The test would fail if lock polling slept on the current runtime
  worker.

## Test plan

- `cargo test -p anie-auth`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke: run two anie processes or a small test harness sharing
  an auth directory, hold the provider lock in one, and confirm the
  second times out without freezing unrelated UI/background activity.

## Risks

- `spawn_blocking` errors must not be flattened into success-shaped
  fallbacks. A join error should become an explicit `RefreshError`
  mapping or a `Persist` error with context.
- The returned locked `File` must stay alive across the refresh and
  persist steps. Do not drop it immediately after acquisition.
- Do not remove the double-check-after-lock behavior; that is the race
  fix.

## Exit criteria

- OAuth refresh still protects cross-process token rotation.
- No long blocking sleep runs on Tokio worker threads.
- Timeout and persistence failures remain typed and actionable.

