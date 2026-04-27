# 01 — Repo formatting and CI hygiene

## Rationale

The review validation found that the codebase is functionally green but
formatting is not:

- `cargo test --workspace` passed.
- `cargo clippy --workspace --all-targets -- -D warnings` passed.
- `cargo fmt --all -- --check` failed.
- `.github/workflows/ci.yml` has a dedicated `fmt` job that runs exactly
  `cargo fmt --all -- --check`.

This must land before behavioral fixes so CI feedback stays meaningful.
A formatting-only commit also keeps later code-review diffs small.

## Design

Run `cargo fmt --all` and commit the resulting mechanical diff without
any behavior changes.

Do not opportunistically edit code while formatting. If a later plan
needs a nearby change, make that change in the later PR.

## Files to touch

Whatever `cargo fmt --all` rewrites. The review observed diffs in at
least:

- `crates/anie-cli/src/controller.rs`
- `crates/anie-cli/src/controller_tests.rs`
- `crates/anie-cli/src/models_command.rs`
- `crates/anie-provider/src/model.rs`
- `crates/anie-providers-builtin/src/local.rs`
- `crates/anie-providers-builtin/src/ollama_chat/mod.rs`
- `crates/anie-tools-web/src/read/*.rs`
- `crates/anie-tools-web/src/search/*.rs`
- `crates/anie-tools-web/tests/fetch_basic.rs`
- `crates/anie-tui/src/app.rs`
- `crates/anie-tui/src/input.rs`
- `crates/anie-tui/src/output.rs`
- `crates/anie-tui/src/overlays/onboarding.rs`

## Phased PRs

### PR A — Format-only cleanup

**Change:**

- Run `cargo fmt --all`.
- Commit only rustfmt output.

**Tests:**

- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

**Exit criteria:**

- Formatting check passes locally.
- No behavior changes are mixed into the formatting commit.

## Risks

- Large formatting diffs can obscure behavioral changes if mixed with
  code edits. Keep this PR isolated.
- If rustfmt version drift appears, use the workspace toolchain
  (`rust-toolchain.toml`) rather than a system-default Rust.

## Deferred

- CI changes. The current CI command is correct; the repository just
  needs to be formatted.
