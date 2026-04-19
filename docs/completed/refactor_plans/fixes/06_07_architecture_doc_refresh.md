# Fix 06/07 — Architecture doc refresh

Lands plan 06 phase 4's architecture-doc note (advisory file lock)
and plan 07 phase 1 sub-step C's architecture-doc pruning
(`anie-extensions` references).

## Motivation

Two plans each called for specific updates to
`docs/arch/anie-rs_architecture.md` and
`docs/arch/anie-rs_build_doc.md`. Neither landed.

### What plan 06 expected

- A paragraph under the session-persistence section noting that
  sessions are opened with an exclusive advisory lock
  (`fd-lock`), that a second opener sees `SessionError::AlreadyOpen`,
  and that the lock degrades cleanly on filesystems that lack
  advisory-lock support (warning + fallback).

### What plan 07 expected

- Remove the `anie-extensions` box from the architecture diagram.
- Remove the `anie-extensions` rows from the event flow (hooks
  like `before_agent_start`, `before_tool_call`, `after_tool_call`).
- Remove `anie-extensions` from the crate dependency list.
- Replace with a one-line pointer to
  `docs/refactor_plans/10_extension_system_pi_port.md` so a
  reader understands the absence is intentional and that a real
  design exists.

### What's actually in the arch doc today

Grep of `docs/arch/anie-rs_architecture.md`:

| Line | Stale reference |
|---|---|
| 40 | `anie-extensions` box in the ASCII diagram |
| 42 | "Extension" in the box body |
| 113 | "`anie-extensions: before_agent_start hook`" in the event flow |
| 142 | "`anie-extensions: before_tool_call`" in the tool-call flow |
| 150 | "`anie-extensions: after_tool_call`" in the tool-call flow |
| 170 | `anie-extensions` in the dependency graph |

Grep of `docs/arch/anie-rs_build_doc.md`:

| Line | Stale reference |
|---|---|
| 39 | `anie-extensions/` in the crate tree |
| 65 | `anie-extensions` in the dependency tree |
| 810–866 | An entire section describing the extensions crate that does not exist |
| 905 | "Extensions can modify the final prompt per-turn..." |
| 937 | "Load config, auth, and extensions." |
| 962, 983, 991 | Event-flow references |
| 1047 | "Post-v1.0: add `anie-extensions` compiled hooks if schedule allows." |

`anie-rs_architecture.md` also does not mention the session
file-lock at all.

## Design principles

1. **Docs match code.** If a crate doesn't exist, the docs don't
   describe it as if it does.
2. **Don't delete context.** Extensions are a real planned feature
   (plan 10). The arch doc should say "extensions are designed as
   a future out-of-process system — see plan 10," not simply
   remove the topic.
3. **Small, self-contained edit.** This is a doc PR, not a
   rewrite. Diff should be surgical.

## Preconditions

- Plan 06 phase 2 (fd-lock + `AlreadyOpen`) landed.
- Plan 07 phase 1 (crate directory deleted) landed.

Both confirmed done.

---

## Phase 1 — `anie-rs_architecture.md`: prune extensions, add lock note

**Goal:** Architecture diagram and event flow reflect the current
crate graph and runtime behavior.

### Files to change

| File | Change |
|------|--------|
| `docs/arch/anie-rs_architecture.md` | Remove 6 `anie-extensions` references; add one paragraph on session locking |

### Sub-step A — Prune the diagram

Find the ASCII box diagram around line 38–55. The `anie-extensions`
box appears on line 40. Remove the box. If removing it makes the
diagram narrower, rebalance the columns. If nothing would fill the
space, leave the space — empty columns are fine.

### Sub-step B — Prune the event flow

Event flow starts around line 108. Remove the three hook lines:

- Line 113: `      ├─► anie-extensions: before_agent_start hook`
  and the following "may modify system_prompt, inject messages"
  continuation.
- Line 142: `      │       ├─► anie-extensions: before_tool_call (can block)`.
- Line 150: `      │       ├─► anie-extensions: after_tool_call (can override result)`.

Adjust tree indentation (`│`, `├─►`, `└─►`) so the remaining
items render correctly.

### Sub-step C — Prune the crate dependency list

Around line 170, the crate list contains `anie-extensions`. Remove
that line. Confirm the arrows below it still point at the correct
dependents.

### Sub-step D — Add the "future extensions" pointer

Add a short note at an appropriate spot (either end of the
"Crates" section or in a dedicated "Future" block):

> **Extensions.** Out-of-process, JSON-RPC extension system
> planned — see
> [`docs/refactor_plans/10_extension_system_pi_port.md`](../refactor_plans/10_extension_system_pi_port.md).
> No extension crate is present in the current workspace; the
> `BeforeToolCallHook` / `AfterToolCallHook` traits in
> `anie-agent/src/hooks.rs` are internal-only seams that will be
> consumed by the extension host when it lands.

### Sub-step E — Add the session-lock paragraph

In the section that describes `anie-session` / session
persistence (around line 88 or line 196), add:

> **Concurrent writers.** A session file is opened with an
> exclusive advisory file lock (via `fd-lock`). A second attempt
> to open the same session returns `SessionError::AlreadyOpen`,
> which the CLI surfaces as an actionable error message and a
> non-zero exit. On filesystems that don't support advisory locks
> (some network filesystems), the lock attempt is a no-op and a
> warning is logged rather than failing hard.

### Exit criteria

- [ ] `grep -in 'anie-extensions' docs/arch/anie-rs_architecture.md`
      returns only the plan-10 pointer.
- [ ] The diagram renders without the `anie-extensions` box.
- [ ] The event flow no longer references the extension hooks.
- [ ] A session-lock paragraph exists.

---

## Phase 2 — `anie-rs_build_doc.md`: align the older build-plan doc

**Goal:** Bring the build-plan doc in sync. This doc is historical
(tracks the v1.0 build plan) but is still linked from the README,
so it should not contradict the working tree.

### Files to change

| File | Change |
|------|--------|
| `docs/arch/anie-rs_build_doc.md` | Mark the `anie-extensions` section as deprecated; add pointers to plan 07 + plan 10; prune the crate tree entries |

### Sub-step A — Decide the doc's audience

Read the top of the file. If it's explicitly "v1.0 build plan,
historical," the surgical fix is:

- Add an explicit note at the top: "Historical — some sections
  (notably `anie-extensions`) describe an early design that was
  reverted. Current state: see
  `docs/arch/anie-rs_architecture.md` and
  `docs/refactor_plans/10_extension_system_pi_port.md`."
- Leave the body intact so git history makes sense.

If it's presented as "current build plan," more pruning is
needed — walk each reference and either remove or contextualize.

Prefer the historical-marker approach unless the doc is clearly
meant as current. This avoids rewriting a document that wasn't
meant to be a live reference.

### Sub-step B — Minimum-viable pruning (if historical)

- Line 39 (crate tree): strike through or remove
  `anie-extensions/` entry, add comment "removed — see plan 07."
- Line 65 (dep tree): same.
- Line 810–866 (the entire `anie-extensions` section): prepend a
  banner "## DEPRECATED — see plan 07 for deletion, plan 10 for
  replacement design."
- Line 1047 (the "Post-v1.0" reference): update to "Post-v1.0:
  implement plan 10 (out-of-process extension system)."

Leave the event-flow references in 962/983/991 alone if the doc
is clearly historical — their narrative makes sense in context.

### Sub-step C — Minimum-viable pruning (if current)

If the doc is presented as current, do all of Sub-step B plus:

- Line 905 ("Extensions can modify the final prompt per-turn..."):
  remove.
- Lines 962/983/991: remove.
- Line 937 ("Load config, auth, and extensions."): drop the "and
  extensions" clause.

### Exit criteria

- [ ] Reader cannot come away thinking `anie-extensions` is a
      current crate.
- [ ] Plan 07 and plan 10 are named as the authoritative design
      for extensions.

---

## Phase 3 — Cross-check README

**Goal:** README hasn't drifted elsewhere while we were looking.

### Files to change

| File | Change |
|------|--------|
| `README.md` | Spot-check for any `anie-extensions` references; confirm the session-lock bullet from plan 06 phase 4 sub-step A is present |

### Sub-step A — Grep

```
grep -n 'anie-extensions\|anie_extensions' README.md
grep -n 'lock\|advisory' README.md
```

Expected:

- Zero `anie-extensions` hits (plan 07 should have pruned them).
- At least one hit for `lock` / `advisory` (plan 06 phase 4
  sub-step A added a sentence at README.md:242).

If either fails, fix in the same PR.

### Exit criteria

- [ ] README contains no `anie-extensions` references.
- [ ] README mentions the session lock.

---

## Phase 4 — Update `docs/refactor_plans/README.md` plan map

**Goal:** The refactor-plans README describes plan 07 as having
"removes the misleading placeholder." Make sure it also notes that
the arch-doc pruning is complete after this fix.

### Files to change

| File | Change |
|------|--------|
| `docs/refactor_plans/README.md` | Add a one-line note under the plan 07 row: "arch-doc pruning landed in `fixes/06_07_architecture_doc_refresh.md`" |
| `docs/refactor_plans/implementation_review_2026-04-18.md` | Strike the plan 06 phase 4 / plan 07 phase 1 sub-step C gaps once this fix lands |

### Exit criteria

- [ ] Plan-map has a forward pointer to this fix plan.
- [ ] Review doc reflects completion.

---

## Test plan (cross-phase)

Docs don't have a compiler, so the test plan is manual:

| # | Test |
|---|------|
| 1 | Open both arch docs in a Markdown preview; confirm no broken links |
| 2 | `grep -rn 'anie-extensions\|anie_extensions' docs/arch/` returns only the pointer-to-plan-10 hits |
| 3 | `grep -rn 'anie-extensions\|anie_extensions' README.md` returns zero hits |
| 4 | `grep -rn 'fd-lock\|advisory' docs/arch/anie-rs_architecture.md` returns at least one hit |
| 5 | Read the arch doc end-to-end; it should read consistently with the current code |

---

## Files that must NOT change

- Source code. This is a docs-only PR.
- `docs/refactor_plans/07_extensions_crate_decision.md` — the plan
  doc itself already documented the intent. Don't rewrite plans
  from history.
- `docs/refactor_plans/10_extension_system_pi_port.md` — leave it
  alone; its internal structure is intentional.

## Dependency graph

```
Phase 1 (arch.md) ──┐
Phase 2 (build_doc.md) ──┼── Phase 3 (README check) ──► Phase 4 (plan-map)
```

All three could be one PR. Split only if reviewer prefers.

## Out of scope

- Writing a fresh `anie-rs_architecture.md`. This is a pruning
  pass, not a rewrite.
- Promoting `anie-rs_build_doc.md` to "deprecated: archived"
  status. If that's desired, do it in a separate PR.
- Adding a new architecture overview for plan 10. That's plan 10's
  own responsibility.
