# anie — Refactor Plans Index

These plans operationalize the findings in
`docs/project_review_2026-04-17.md`. Each plan follows the same
template used by `docs/reasoning_fix_plan.md`:

- A short root-cause / motivation section
- Explicit phases, each touching **≤5 files**
- A **Files to change** table per phase
- Per-phase **Test plan** (numbered) and **Exit criteria** (checklist)
- A **Files that must NOT change** section where relevant
- A final **Out of scope** section

## Plans

| # | Title | Scope | Payoff |
|---|---|---|---|
| 00 | [CI enforcement](./00_ci_enforcement.md) | Tiny — `.github/workflows/ci.yml` only | Prevents regressions in all other refactors |
| 01 | [openai.rs module split + streaming tests](./01_openai_module_split.md) | `anie-providers-builtin` | Unblocks `reasoning_fix_plan.md` Phase 1; biggest single file win |
| 02 | [TUI overlay trait + shared widgets](./02_tui_overlay_trait.md) | `anie-tui` | Deletes the onboarding ↔ providers duplication |
| 03 | [Controller decomposition](./03_controller_decomposition.md) | `anie-cli`, small `anie-session` boundary touch | Retires the `ControllerState` God object |
| 04 | [Provider HTTP + discovery unification](./04_provider_http_unification.md) | `anie-providers-builtin` | Deletes the OpenAI ↔ Anthropic duplication |
| 05 | [Provider error taxonomy](./05_provider_error_taxonomy.md) | `anie-provider`, `anie-providers-builtin`, callers | Eliminates string-typed error API |
| 06 | [Session write locking](./06_session_write_locking.md) | `anie-session` | Prevents multi-process corruption |
| 07 | [`anie-extensions` decision](./07_extensions_crate_decision.md) | `anie-extensions`, `anie-agent` | Removes placeholder or makes it real |
| 08 | [Small hygiene items](./08_small_hygiene_items.md) | Cross-cutting | Cheap wins: `.anie/` paths, `.expect` audit, event-send logging, clone audit, context API |
| — | [pi-mono comparison](./pi_mono_comparison.md) | — | How these plans map against pi's actual architecture; flags feature gaps |

## Recommended order

The dependency graph:

```
00 (CI)
  └── blocks nothing, enables everything below

01 (openai split)     ◄─ high priority: unblocks reasoning_fix_plan
  └── 04 (provider unification) relies on the module layout from 01
  └── 05 (error taxonomy) is easier after 01 because call sites are localized

02 (TUI overlay)       ◄─ high priority: stops bleeding in onboarding ↔ providers

03 (controller split)  ◄─ medium priority: independent of 01/02/04/05
  └── 08.E (cached ToolRegistry) lands here

04 (provider unify)    ◄─ medium priority: after 01
05 (error taxonomy)    ◄─ medium priority: after 01; touches many sites

06 (session locking)   ◄─ low priority, but small; do anytime
07 (extensions)        ◄─ decide in one sitting; land the decision
08 (hygiene)           ◄─ pick items opportunistically
```

Suggested pacing:

- **Week 1:** 00 (ten minutes), then 01.
- **Week 2:** 02.
- **Week 3:** 03, plus 06 and 07 as side work.
- **Week 4:** 04 and 05 together (they share the openai.rs surface
  area touched by 01).
- **Ongoing:** 08 items are pickable one at a time.

## How these plans relate to existing plans

- `docs/reasoning_fix_plan.md` — Phase 1 becomes cheaper after plan
  01. Phase 3 already covers the "scattered reasoning capabilities"
  item from the review; that work is not duplicated here.
- `docs/integration_testing_plan.md`, `docs/testing_phases/*` — the
  new unit tests added by plan 01 complement (not replace) the
  integration coverage.
- `docs/ideas.md` — items like `/settings`, inline command menus,
  skills become materially cheaper to implement after plans 02 and
  03 land.

## Not in scope for any of these plans

- Sandboxing and tool approvals (tracked separately).
- OAuth and subscription auth (tracked in `docs/ideas.md`).
- New features from `docs/ideas.md` that aren't cleanup.
- Performance micro-optimization below the algorithmic level.
