# Plan 00 — CI Enforcement

This is a ten-minute plan. Land it first. It protects every other
plan from silently accumulating warnings.

## Motivation

`.github/workflows/ci.yml` currently runs `cargo build --release` and
`cargo test --workspace`. The `Makefile` also defines `clippy` (with
`-D warnings`) and `fmt` (`--check`), but those are not enforced in
CI. The workspace lint config is strict (`unwrap_used = "warn"`,
`redundant_clone = "deny"`, `uninlined_format_args = "deny"`,
`manual_let_else = "deny"`), but a PR that introduces a new warning
will still merge.

## Design principles

1. **The Makefile is the source of truth for developer workflow.** CI
   should run the same commands developers already run.
2. **Fail fast, fail specific.** `clippy` and `fmt` should be their
   own jobs so a formatting miss is visibly distinct from a build
   failure.
3. **No matrix duplication.** Run `clippy` and `fmt` on Linux only;
   keep the existing Linux/macOS/Windows matrix for build+test.

---

## Phase 1 — Add lint and format gates to CI

**Goal:** CI fails on any new clippy warning or formatting drift.

### Files to change

| File | Change |
|------|--------|
| `.github/workflows/ci.yml` | Add `clippy` and `fmt` jobs alongside existing `test` matrix |

### Sub-step A — Add a `fmt` job

Add a new job that runs only on Linux:

```yaml
fmt:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
      with:
        components: rustfmt
    - run: cargo fmt --all -- --check
```

### Sub-step B — Add a `clippy` job

```yaml
clippy:
  runs-on: ubuntu-latest
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
      with:
        components: clippy
    - uses: Swatinem/rust-cache@v2
    - run: cargo clippy --workspace --all-targets -- -D warnings
```

### Sub-step C — Keep the existing matrix

The `test` job stays as-is. Do not merge `clippy` into the matrix —
running clippy three times on three OSes is wasteful and the lint
result is OS-independent.

### Files that must NOT change

- `Makefile` — the targets already exist and match the CI commands.
- `Cargo.toml` (workspace) — lint config is already correct.
- Any crate source — this plan adds no code changes.

### Test plan

| # | Test |
|---|------|
| 1 | Open a draft PR that intentionally violates a clippy rule (e.g., add a `.clone()` that triggers `redundant_clone`). Confirm the `clippy` job fails. |
| 2 | Open a draft PR that intentionally mis-indents a Rust file. Confirm the `fmt` job fails. |
| 3 | Confirm the `test` job still runs on the full OS matrix and is unaffected. |

### Exit criteria

- [ ] `clippy` job fails CI on any warning, workspace-wide, all targets.
- [ ] `fmt` job fails CI on any formatting drift.
- [ ] `test` job matrix is unchanged.
- [ ] Both new jobs run in parallel with `test`.
- [ ] Each new job uses `Swatinem/rust-cache@v2` where applicable.

---

## Out of scope

- Adding a `secret-scan` dependency between jobs (it's already its own
  workflow).
- Release automation.
- Coverage reporting.
- MSRV pinning in CI (the `rust-toolchain.toml` already pins via
  `rust-version = "1.85"` in `Cargo.toml`; a dedicated MSRV job can be
  added later if needed).
