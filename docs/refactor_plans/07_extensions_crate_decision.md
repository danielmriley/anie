# Plan 07 — `anie-extensions` decision

> **Revised 2026-04-17.** After reviewing pi-mono's real extension
> system (see `pi_mono_comparison.md`), the original "Option B —
> make it real" section is superseded by **plan 10** (full
> pi-shaped extension port). Plan 07 now delivers Option A only:
> remove the current stub crate, so plan 10 can rebuild the crate
> from scratch against a proper contract.

## Motivation

`crates/anie-extensions/src/lib.rs` is four lines:

```rust
//! Extension hooks for anie-rs.

/// Placeholder to keep the crate compiling during workspace bootstrap.
pub const CRATE_NAME: &str = "anie-extensions";
```

The architecture doc (`docs/arch/anie-rs_architecture.md`)
describes an `Extension` trait with `before_agent_start`,
`session_start`, `before_tool_call`, `after_tool_call`. None of
that exists. `crates/anie-agent/src/hooks.rs` defines
`BeforeToolCall` / `AfterToolCall` traits, but they're always
constructed as `None` in `AgentLoopConfig` and never wired to this
crate.

The stub crate signals "this is a real extension point" when it
isn't. A half-hearted fix would lock in a too-small contract: pi's
extension surface (`pi_mono_comparison.md`) covers 35+ event
types, tool/command/shortcut/flag/provider/message-renderer
registration, and a rich UI context — 3099 LOC total. A 4-hook
compiled-in trait does not reach that bar and would need to be
redesigned when the real port begins.

Therefore: delete the stub now. Rebuild in plan 10.

## Design principles

1. **Remove the placeholder.** It misleads code readers.
2. **Don't block plan 10.** Nothing this plan does should create
   work for plan 10 to undo.
3. **Keep the name available.** `anie-extensions` will be recreated
   in plan 10 phase 1 with real contents.

---

## Phase 1 — Remove `anie-extensions`

**Goal:** The workspace no longer has `anie-extensions`.

### Files to change

| File | Change |
|------|--------|
| `Cargo.toml` (workspace) | Remove `"crates/anie-extensions"` from `members`; remove the `anie-extensions` entry from `[workspace.dependencies]` |
| `crates/anie-extensions/` | Delete directory |
| `crates/anie-agent/Cargo.toml` | Confirm no dep on `anie-extensions`; remove if present |
| `crates/anie-cli/Cargo.toml` | Same |
| `docs/arch/anie-rs_architecture.md` | Delete the "Extensions" block in the diagram; note in prose that extensions are a v2 concept (tracked in plan 10) |

### Sub-step A — Confirm nothing depends on it

Run `grep -rn 'anie-extensions\|anie_extensions' crates/`. The only
hits should be the crate's own files. If anything else references
it, either inline the reference or update this plan before
proceeding.

### Sub-step B — Delete and verify

Delete the directory. `cargo build --workspace` and
`cargo test --workspace` must still pass.

### Sub-step C — Architecture doc update

In `docs/arch/anie-rs_architecture.md`:

- Remove the `anie-extensions` box from the high-level diagram.
- Add a one-line note under the diagram: "Extensions are designed
  as a future out-of-process plugin system; see
  `docs/refactor_plans/10_extension_system_pi_port.md`."

### Test plan

| # | Test |
|---|------|
| 1 | `cargo build --workspace` passes |
| 2 | `cargo test --workspace` passes |
| 3 | `grep -rn 'anie[-_]extensions' crates/` returns nothing |
| 4 | `cargo clippy --workspace --all-targets -- -D warnings` passes |

### Exit criteria

- [ ] Crate directory removed.
- [ ] Workspace `Cargo.toml` cleaned.
- [ ] Arch doc updated; points to plan 10.
- [ ] No stale references anywhere in `crates/` or `docs/`.

---

## Phase 2 — Acknowledge `anie-agent/src/hooks.rs`

**Goal:** `hooks.rs` is currently defined but never used from
outside the crate. Decide its status explicitly.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-agent/src/hooks.rs` | Either mark the traits `pub(crate)` (internal-only), OR add a module-level doc comment explaining the traits are reserved for plan 10 |

### Sub-step A — Recommended path

Mark the traits `pub(crate)`. When plan 10 phase 3 lands, it will
either:

- reuse these traits by forwarding from the `ExtensionHost` to the
  internal hooks, or
- replace them with a new internal interface designed against the
  JSON-RPC event surface.

Either path starts cleanly from "internal to the crate." Keeping
them `pub` today suggests a contract that doesn't exist.

### Sub-step B — Document intent

Add a module-level doc comment:

```rust
//! Internal hook traits used by the agent loop.
//!
//! These are `pub(crate)` by design — a public extension API is
//! planned separately as an out-of-process JSON-RPC system; see
//! `docs/refactor_plans/10_extension_system_pi_port.md`. Do not
//! expose these outside the crate without updating that plan.
```

### Exit criteria

- [ ] `hooks.rs` traits are `pub(crate)`.
- [ ] Module doc comment points at plan 10.
- [ ] No external crate imports from `hooks.rs`.

---

## Files that must NOT change

- `crates/anie-protocol/*` — no protocol change.
- `crates/anie-tui/*` — no TUI change.
- `crates/anie-session/*` — sessions are not an extension consumer.

## Dependency graph

```
Phase 1 ──► Phase 2
```

Both are small. Can land in a single PR.

## Relationship to plan 10

Plan 10 (extension system pi port) **assumes** plan 07 has landed.
It will recreate `crates/anie-extensions/` from scratch with real
contents. Do not skip ahead — landing plan 10 on top of the stub
only adds cleanup noise.

## Out of scope

- Anything about the actual extension system design — that's plan
  10.
- Dynamic extension loading, WASM, Starlark — all plan 10 or
  later.
- Moving existing `hooks.rs` traits into a public location — they
  stay internal until plan 10 redesigns them.
