# Local-model augmentation — execution tracker

Status of the PRs defined in
[`docs/local_model_augmentation/`](../README.md). Update as
PRs land (convention: link the commit hash, note deviations
from plan inline).

Recommended landing order: plan 01 → plan 03 → plan 02.
`RunMetrics` schema bumps (01/PR4 and 03/PR4) must land
sequentially, not in parallel.

## Plan 01 — Tool-call reliability

| PR | Title | Status | Commit |
|----|-------|--------|--------|
| PR1 | Schema-guided tool-argument coercion | Done | `6d5e96e` |
| PR2 | Bounded tool-call repair round | Done | `d8aa8f6` |
| PR3 | Per-tool example calls in prompt catalog | Done | `d6aaea9` |
| PR4 | Tool-reliability metrics + eval scenarios | Done | `7894918` |

Plan-01 deviations (rationale in the commits / code comments):

- PR1 validates strictly first and coerces only on failure
  (zero happy-path overhead), instead of coercing before
  validation.
- PR2 implements repair as a side request rather than the
  ideas-doc `AgentIntent::RepairToolCall` variant.
- PR3 uses a static example map in `anie-cli/src/tool_examples.rs`
  instead of a `ToolDef.example` field (~55 construction sites
  untouched; wire leak structurally impossible). The
  schema-validation drift guard caught the plan doc's own wrong
  `edit` example.
- Pending live verification: the two qwen3:8b smoke items in the
  plan's exit criteria (coercion + repair against a real model).

## Plan 02 — Repo map + retrieval

| PR | Title | Status | Commit |
|----|-------|--------|--------|
| PR5 | Repo-map builder (tree + regex signatures + git log) | Not started | — |
| PR6 | RepoMapPolicy injection at first model turn | Not started | — |
| PR7 | repo_map drill-down tool | Not started | — |
| PR8 | Repo-map eval scenarios | Not started | — |

## Plan 03 — Harness verification + failure recovery

| PR | Title | Status | Commit |
|----|-------|--------|--------|
| PR9 | `[verify]` config + harness-run verification policy | Not started | — |
| PR10 | Forward temperature to Ollama + loop perturbation | Not started | — |
| PR11 | Grounded edit-failure recovery | Not started | — |
| PR12 | Recovery metrics + broken-fixture eval scenarios | Not started | — |

## Measurement log

Record map-on/off and feature-on/off corpus deltas here as PR4
/ PR8 / PR12 land (model, scenario family, pass-rate, total
tokens, tool calls, turns).

| Date | Feature | Model | Corpus result | Notes |
|------|---------|-------|---------------|-------|
| — | — | — | — | — |
