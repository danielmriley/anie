# 04 — OAuth callback per-connection deadlines

## Rationale

`await_callback_on_path` has an overall deadline for accepting a
connection, but after `listener.accept()` it reads from the accepted
stream without a per-connection timeout. A local process can connect to
the callback port and send no bytes, leaving the login flow waiting on
that accepted socket instead of respecting the overall OAuth timeout.

The pi comparison showed token exchange requests use explicit timeout
signals. The Codex comparison initially appeared to show socket
timeouts, but direct verification showed the cited 2 second timeout is
on Codex's cancel-request client, not necessarily on accepted callback
sockets. That lowers the reference confidence but not the anie bug: the
accepted stream still needs deadline enforcement.

## Design

Use the already-computed overall deadline as the source of truth:

- Wrap accepted-stream request reads in `tokio::time::timeout(remaining,
  stream.read(...))`.
- Wrap response writes in the remaining deadline or a short
  per-connection timeout.
- If a non-callback connection times out or is malformed, close it and
  continue listening until the overall deadline expires.
- If the actual callback path times out mid-read, treat it as a bad
  local connection and continue unless the overall deadline has elapsed.

Avoid OS-specific socket timeout APIs for Tokio streams unless there is
a compelling reason. Tokio timeouts are portable and match the existing
async style.

## Files to touch

- `crates/anie-auth/src/callback.rs`
  - Add helpers for deadline-bounded read and write.
  - Ensure malformed/idle connections do not consume the whole login
    flow.
- Auth callback tests
  - Add idle-connection regression coverage.

## Phased PRs

### PR A — Deadline-bound accepted reads

**Change:**

- Compute remaining time after `accept`.
- Wrap `stream.read(&mut buffer)` in `tokio::time::timeout`.
- On timeout, close the stream and continue to the next accept loop
  iteration if the overall deadline remains.

**Tests:**

- Open a TCP connection to the callback server and send no bytes.
- Then send a valid callback on a second connection.
- Assert the valid callback succeeds.

**Exit criteria:**

- An idle local connection cannot hang login indefinitely.

### PR B — Deadline-bound response writes

**Change:**

- Wrap `write_http_response` calls with the same deadline discipline.
- Ignore failed writes only after recording enough context for logs or
  tests, consistent with current callback behavior.

**Tests:**

- Malformed request still gets a best-effort 400 when possible.
- Write timeout does not prevent the server from accepting a later valid
  callback before the overall deadline.

**Exit criteria:**

- Both read and write phases respect bounded time.

## Test plan

- `cargo test -p anie-auth callback`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke: start login, connect to the callback port with an idle
  client, then complete browser callback and confirm login finishes.

## Risks

- Too-short per-connection windows can break slow browsers or security
  software. Prefer the remaining overall deadline unless a smaller
  timeout is well justified.
- Do not convert all malformed requests into fatal login failures;
  browsers may request `/favicon.ico`.

## Exit criteria

- Overall OAuth timeout is honored even after accepting a bad local
  connection.
- Valid callbacks still succeed after unrelated malformed or idle
  connections.

