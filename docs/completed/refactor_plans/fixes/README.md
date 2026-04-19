# Refactor-plan fix plans

These plans address the gaps called out in
`docs/refactor_plans/implementation_review_2026-04-18.md`. Each one
fills in work that `docs/refactor_plans/00–08` specified but did not
fully land. They follow the same template as the original plans:

- Motivation / background
- Design principles (where relevant)
- Explicit phases, each touching **≤5 files**
- Per-phase **Files to change** table, **Sub-steps**, **Test plan**,
  and **Exit criteria**
- A **Files that must NOT change** section where meaningful
- **Out of scope** at the end

## Index

| # | Title | Parent plan | Priority | Effort |
|---|---|---|---|---|
| 01 | [Colocate openai submodule tests](./01_colocate_openai_submodule_tests.md) | 01 | low | ~2 hr |
| 02a | [Overlay placeholder stubs](./02a_overlay_placeholder_stubs.md) | 02 Phase 6 Sub-step B | medium | ~1 hr |
| 02b | [Finish clone audit + typed provider key](./02b_finish_clone_audit.md) | 02 Phase 4 | low | ~2 hr |
| 03a | [Slash-command dispatch (finish Phase 3)](./03a_slash_command_dispatch.md) | 03 Phase 3 | **high** | ~1 day |
| 03b | [RetryPolicy extraction + taxonomy reconciliation](./03b_retry_policy_extraction.md) | 03 Phase 4 + 05 | **high** | ~half day |
| 03c | [ConfigState + controller.rs shrink (finish Phase 5)](./03c_finish_controller_split.md) | 03 Phase 5 | **high** | ~1 day |
| 06-07 | [Architecture doc refresh](./06_07_architecture_doc_refresh.md) | 06 Phase 4 + 07 Phase 1 Sub-step C | medium | ~1 hr |
| 07 | [hooks.rs visibility narrowing](./07_hooks_visibility.md) | 07 Phase 2 | low | ~15 min |
| 08 | [Plan-status hygiene + missing Phase D tests](./08_status_hygiene_and_tests.md) | 08 | low | ~1 hr |

## Recommended order

```
low-hanging fruit (one PR each, <30 min):
  07 (hooks visibility)
  08 (status hygiene)
  06-07 (arch doc refresh)
  02a (overlay stubs)

medium follow-ups:
  01 (test colocation) — independent
  02b (clone audit)    — independent

high-value, independent:
  03a (slash-command dispatch)    — builds metadata registry into /help
  03b (RetryPolicy extraction)    — consolidates retry + reconciles plan 05
  03c (ConfigState + controller.rs shrink) — depends on 03b having landed
```

03a/03b/03c together discharge the bulk of the debt called out in the
review. They can be done in any order, except **03c should follow
03b** — the retry-related shape of `ControllerState` is simpler to
finalize after `RetryPolicy::decide` exists.

## How these plans relate to the originals

Each plan here explicitly cites which original phase it finishes or
corrects. None of them introduce new product surface — they close out
work the original plans promised but left partial. If a fix plan
disagrees with its parent plan (e.g., 03a adopts a narrower trait
shape than the original proposed), the disagreement is called out
and justified in the "Divergence from parent plan" section.

## Not in scope for these plans

- Plan 10 (extension system) — out of scope here; plan 10 itself is
  a multi-week new feature, not a fix.
- New first-party features from `docs/ideas.md`.
- Anything not already called out in
  `docs/refactor_plans/implementation_review_2026-04-18.md`.
