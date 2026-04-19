# Fix 07 — Narrow `anie-agent` hook traits to `pub(crate)`

One-phase, ten-minute fix for plan 07 phase 2.

## Motivation

Plan 07 phase 2 called for the hook traits in
`crates/anie-agent/src/hooks.rs` to be narrowed from `pub` to
`pub(crate)`. Exit criterion:

> hooks.rs traits are pub(crate).

What landed: the module-level doc comment was updated to point at
plan 10, but the visibility modifier stayed `pub`.

**Why it matters:**

- Public visibility advertises "this is a supported API."
- The hook traits are not consumed outside `anie-agent` anywhere in
  the workspace (verified by `grep`).
- Leaving them `pub` implies a contract that plan 10 will redesign,
  not preserve. A user who depends on them today will be surprised
  when plan 10 lands.

## Design principles

1. **Minimum visibility by default.** Public API is a commitment.
   If no consumer needs it, don't advertise it.
2. **Zero behavior change.** This is a `pub` → `pub(crate)`
   modifier flip. Nothing else moves.

## Preconditions

- Plan 07 phase 1 (crate delete) landed.
- `crates/anie-agent/src/hooks.rs` contains the traits.

Both confirmed.

## Current state

`hooks.rs` currently exposes:

| Item | Visibility |
|---|---|
| `BeforeToolCallResult` | `pub` enum |
| `ToolResultOverride` | `pub` struct |
| `BeforeToolCallHook` | `pub` trait |
| `AfterToolCallHook` | `pub` trait |

All are re-exported from `crates/anie-agent/src/lib.rs`. External
callers of the crate can (but don't) name them.

Workspace-wide consumers (grep):

- `crates/anie-agent/src/agent_loop.rs` — uses all four.
- `crates/anie-agent/src/tests.rs` — uses `BeforeToolCallResult`,
  `BeforeToolCallHook`, `AfterToolCallHook`.
- `crates/anie-agent/src/hooks.rs` — defines.

No consumer outside `anie-agent`. Verified with:

```bash
grep -rn 'BeforeToolCallHook\|AfterToolCallHook\|BeforeToolCallResult\|ToolResultOverride' crates/
```

---

## Phase 1 — Narrow visibility

**Goal:** Each hook item is `pub(crate)`. The `lib.rs` re-exports
either gate on `#[cfg(test)]` or are removed entirely.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-agent/src/hooks.rs` | `pub` → `pub(crate)` on all four items + fields/variants as needed |
| `crates/anie-agent/src/lib.rs` | Drop the public re-exports; keep any internal re-exports |
| `crates/anie-agent/src/agent_loop.rs` | If it imports via `use crate::*`, nothing changes; if it imports via the re-export, update to `crate::hooks::*` |
| `crates/anie-agent/src/tests.rs` | Same — adjust imports if needed |

(4 files, under the cap.)

### Sub-step A — Identify variant/field visibility

The enum `BeforeToolCallResult` has two variants:

```rust
pub enum BeforeToolCallResult {
    Allow,
    Block { reason: String },
}
```

Variants inherit the enum's visibility — once the enum is
`pub(crate)`, the variants are too. The `reason` field inside
`Block` needs an explicit `pub` only if it's read from outside the
module. It is — `agent_loop.rs` pattern-matches `Block { reason
}`. Mark field `pub(crate)`:

```rust
pub(crate) enum BeforeToolCallResult {
    Allow,
    Block { reason: String },
}
```

Fields of struct-like variants are accessible to code that can see
the enum — the `pub` on the field itself is only needed for
structs. Confirm this (Rust rule) during the change; if the
compiler errors, mark fields explicitly.

Same treatment for `ToolResultOverride`:

```rust
pub(crate) struct ToolResultOverride {
    pub(crate) content: Option<Vec<ContentBlock>>,
    pub(crate) details: Option<serde_json::Value>,
    pub(crate) is_error: Option<bool>,
}
```

### Sub-step B — Narrow the traits

```rust
#[async_trait]
pub(crate) trait BeforeToolCallHook: Send + Sync { ... }

#[async_trait]
pub(crate) trait AfterToolCallHook: Send + Sync { ... }
```

Trait methods' visibility stays `pub` within the trait body
(that's how trait items are declared). No change there.

### Sub-step C — Clean `lib.rs`

Current `lib.rs` re-exports (grep to confirm):

```rust
pub use crate::hooks::{
    AfterToolCallHook, BeforeToolCallHook, BeforeToolCallResult,
    ToolResultOverride,
};
```

Since none of these are consumed outside the crate, delete the
re-export. If agent_loop.rs imports them via `use crate::{...}`,
update to `use crate::hooks::{...}`.

### Sub-step D — Update the module doc comment

Current top-of-file says the traits are "reserved for the planned
out-of-process JSON-RPC extension system." Add:

```rust
//! Tool-execution hook traits used by the agent loop.
//!
//! These traits are `pub(crate)` by design. Today they are
//! consumed only by `agent_loop.rs`. A public extension API is
//! planned separately as an out-of-process JSON-RPC system; see
//! `docs/refactor_plans/10_extension_system_pi_port.md`.
//!
//! If you want to expose these to another crate, update plan 10
//! first — the JSON-RPC shape is the supported path.
```

### Test plan

| # | Test |
|---|------|
| 1 | `cargo build --workspace` passes |
| 2 | `cargo test --workspace` passes |
| 3 | `cargo clippy --workspace --all-targets -- -D warnings` passes |
| 4 | `grep -rn 'BeforeToolCallHook\|AfterToolCallHook\|BeforeToolCallResult\|ToolResultOverride' crates/` returns zero hits outside `anie-agent` |
| 5 | Nothing in `lib.rs` re-exports these symbols publicly |

### Exit criteria

- [ ] All four items are `pub(crate)`.
- [ ] `lib.rs` does not publicly re-export them.
- [ ] Workspace compiles, tests pass, clippy is clean.
- [ ] Module doc comment reflects the updated visibility
      expectation.

---

## Files that must NOT change

- `crates/anie-agent/src/agent_loop.rs` — only imports change (if
  at all); behavior stays identical.
- `crates/anie-agent/src/tests.rs` — imports only.
- Any file outside `anie-agent` — this change has zero external
  impact by construction.

## Dependency graph

Single phase. No ordering constraint beyond its own preconditions.

## Complication — `AgentLoopConfig` exposes the traits publicly

`AgentLoopConfig` is `pub` and has these fields:

```rust
pub before_tool_call_hook: Option<Arc<dyn BeforeToolCallHook>>,
pub after_tool_call_hook: Option<Arc<dyn AfterToolCallHook>>,
```

Three external call sites (`anie-cli/src/controller.rs`,
`anie-tools/src/tests.rs`, `anie-integration-tests/src/helpers.rs`)
set them to `None` and nothing else. No external caller constructs
a real hook.

Narrowing the traits to `pub(crate)` while leaving these fields
`pub` is a "private type in public interface" violation — the
compiler will reject it.

**Resolution:** remove the fields from `AgentLoopConfig` and move
them to an `AgentLoopConfig::with_hooks(...) -> Self` builder
method that only internal callers (`anie-agent::tests.rs`) use.
Three external call sites simplify — they were passing `None`
anyway.

### Additional files to change

| File | Change |
|------|--------|
| `crates/anie-agent/src/agent_loop.rs` | Drop `before_tool_call_hook` / `after_tool_call_hook` from the struct literal fields; add `pub(crate) fn with_hooks(mut self, before: Option<Arc<dyn BeforeToolCallHook>>, after: Option<Arc<dyn AfterToolCallHook>>) -> Self` that stores them in private fields |
| `crates/anie-agent/src/tests.rs` | Switch from field assignment to `.with_hooks(Some(before), Some(after))` |
| `crates/anie-cli/src/controller.rs:955–956` | Delete the two `None` assignments |
| `crates/anie-tools/src/tests.rs:530–531` | Delete |
| `crates/anie-integration-tests/src/helpers.rs:71–72` | Delete |

Five additional files — this fix exceeds the 5-file cap set at the
top of the plan. Split: land the `AgentLoopConfig` reshape +
internal tests in one PR, the three external deletions in a
follow-up. The external PR is mechanical.

## Out of scope

- Designing the public extension API. That's plan 10 phase 3.
- Renaming any of the hook types.
- Adding new hook points.
