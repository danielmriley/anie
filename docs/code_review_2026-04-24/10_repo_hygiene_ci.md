# 10 — Repo hygiene, CI, and print-mode error visibility

## Rationale

The review's low-priority findings are individually small but worth
collecting into one hygiene track:

- local/generated review artifacts can be accidentally committed
- workspace repository metadata points to a placeholder URL
- CI installs stable Rust but does not explicitly check declared MSRV
- secret-scan downloads Gitleaks without checksum verification
- print mode can stream partial assistant output and then hide final
  error context

None of these justify a large refactor. They are best landed as small,
independent PRs after the higher-priority runtime/provider fixes.

## Design

Keep this track as a series of independent cleanup PRs. Do not mix CI
workflow changes with print-mode behavior unless the final branch is
small enough to review comfortably.

## Files to touch

- `.gitignore`
  - Decide which local review artifacts should be ignored.
- `Cargo.toml`
  - Replace placeholder repository metadata or remove it.
- `.github/workflows/ci.yml`
  - Add explicit MSRV check for `rust-version = "1.85"`.
- `.github/workflows/secret-scan.yml`
  - Pin and verify Gitleaks archive checksum, or use a pinned trusted
    action.
- `crates/anie-cli/src/print_mode.rs`
  - Emit final error marker when streamed output ends in error.
- CLI tests
  - Add print-mode error visibility coverage.

## Phased PRs

### PR A — Repository artifact hygiene

**Change:**

- Decide whether generated review artifacts should be committed docs or
  ignored local files.
- Add ignore rules for truly local artifacts only.
- Do not ignore intentional review docs that are part of this plan set.

**Tests:**

- `git status --short` shows no newly ignored intended docs.

**Exit criteria:**

- Local lock/input artifacts are less likely to be committed
  accidentally.

### PR B — Workspace metadata and MSRV CI

**Change:**

- Set `repository = "https://github.com/danielmriley/anie"` or remove
  the placeholder field.
- Add CI job or matrix entry that installs Rust 1.85 and runs
  `cargo check --workspace --all-targets`.

**Tests:**

- `cargo check --workspace --all-targets` on current toolchain.
- CI validates the MSRV job after push.

**Exit criteria:**

- Metadata no longer points at `example.com`.
- CI catches accidental use of APIs newer than the declared MSRV.

### PR C — Secret-scan supply-chain hardening

**Change:**

- Pin Gitleaks download checksum, or replace manual download with a
  pinned action that has an acceptable trust model.

**Tests:**

- Secret-scan workflow succeeds in CI.

**Exit criteria:**

- The workflow no longer executes an unverified downloaded archive.

### PR D — Print-mode final error marker

**Change:**

- If text deltas streamed and final assistant stop reason is error,
  print a concise marker to stderr.
- Preserve successful streaming stdout behavior.

**Tests:**

- Streamed successful assistant output remains unchanged.
- Streamed partial output followed by error prints final error context
  to stderr.

**Exit criteria:**

- Print mode no longer hides final error context after partial output.

## Test plan

- `cargo test -p anie-cli print`
- `cargo check --workspace --all-targets`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- CI verification for MSRV and secret-scan workflow changes.

## Risks

- `.gitignore` rules can accidentally hide files that should be
  committed. Keep patterns narrow.
- MSRV checks can slow CI. Make the job targeted to `cargo check`.
- Error markers must go to stderr so successful stdout consumers are not
  broken.

## Exit criteria

- Repository metadata and hygiene no longer contain known review issues.
- CI verifies the declared MSRV and safer secret scan setup.
- Print-mode failures are visible even after streamed partial output.

