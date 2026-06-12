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
| PR5 | Repo-map builder (tree + regex signatures + git log) | Done | — |
| PR6 | RepoMapPolicy injection at first model turn | Done | — |
| PR7 | repo_map drill-down tool | Done | — |
| PR8 | Repo-map eval scenarios | Done | — |

Plan-02 deviations (rationale in the code comments):

- PR5: the pure builder lives in `anie_tools::repo_map`, not
  `anie-cli/src/repo_map.rs` as planned — anie-cli carries neither
  the `ignore` nor the `regex` dep, and the drill-down helpers
  (`extract_signatures`, `dir_overview`) belong beside the builder.
  Signature extraction iterates lines with `regex` directly rather
  than driving `grep-searcher` (identical result, less sink
  boilerplate). anie-cli owns the policy/tool/gating wrappers in
  its own `repo_map.rs`.
- PR6: the token budget is tier-aware per plan 04 — Small tier
  defaults to 1000 tokens, Full tier to the plan's 2000;
  `ANIE_REPO_MAP_TOKENS` overrides both. Injection is once per
  *session context* (skipped whenever an assistant message exists),
  not literally once per run: re-injecting mid-transcript on a
  continuation would break the prompt prefix.
- PR7: tool co-located in anie-cli (the plan's allowed alternative)
  so it can share the session-scoped cache with the policy via
  `ControllerState`.
- PR8: `find_provider_trait` was already tightened by plan-04 PR15;
  this PR tightened `locate_budget_policy` and
  `find_compaction_stats` (90k/80k → 24k) and added
  `repo_map_cold_start` on the new non-anie `mini_tasks` fixture.
  Map-on vs map-off corpus deltas (exit criterion) still need a
  live gemma4:e4b run — record them in the measurement log below.

## Plan 03 — Harness verification + failure recovery

| PR | Title | Status | Commit |
|----|-------|--------|--------|
| PR9 | `[verify]` config + harness-run verification policy | Done | — |
| PR10 | Forward temperature to Ollama + loop perturbation | Done | — |
| PR11 | Grounded edit-failure recovery | Done | — |
| PR12 | Recovery metrics + broken-fixture eval scenarios | Done | — |

Plan-03 deviations (rationale in the code comments):

- PR9: `VerifyPolicy` baselines at policy-construction time, so a
  resumed session's old successful edits never fire a surprise
  verify at step 0. The timeout group-kill shells out to the POSIX
  `kill` utility — anie-cli carries no `nix`/`libc` dependency
  (`bash.rs` does the same kill via `nix` inside anie-tools).
  Verify outcomes also surface as `[verify]`-prefixed
  `SystemMessage` events (transcript visibility + the PR12
  metrics source).
- PR12: one combined schema bump v3→v4 covering both
  `recovery {…}` (this PR) and `prompt.system_prompt_tokens`
  (plan 04 PR15) — the bumps were coordinated as planned, just in
  a single version. `loop_perturbations` counts the failure-loop
  detector's `[loop warning]` SystemMessage (the observable signal
  at the crossing that arms the perturbation); similar-call
  streak-5 escalations emit no event and are not counted, and
  `ANIE_LOOP_PERTURB=0` keeps the warning, so the counter reads
  "crossings", not literal temperature bumps.
- PR12 scenario: `verify_broken_fixture` needs `--modes rlm` and a
  live model (the verify policy is rlm-gated); the fixture carries
  its own `.anie/config.toml` to arm `[verify]` via project-config
  discovery. The loop-trap fixture from the plan is deferred to
  PR10/PR11 landing (it exercises the perturbation + grounding
  paths).

## Measurement log

Record map-on/off and feature-on/off corpus deltas here as PR4
/ PR8 / PR12 land (model, scenario family, pass-rate, total
tokens, tool calls, turns).

| Date | Feature | Model | Corpus result | Notes |
|------|---------|-------|---------------|-------|
| 2026-06-12 | Full campaign (PR13-19) | gemma4:e4b | time-scenario: prompt 11.3k→1,398 tok, wall 127s→26s | field_notes/2026-06-12_gemma4_baseline.md |
| 2026-06-12 | Full campaign (PR13-19) | qwen3.5:0.8b | time-scenario: failures 36%→17%, hallucinated tools 6→0, answered correctly via bash `date` | same |

## Plan 04 — Context-budget discipline (added 2026-06-12)

| PR | Title | Status | Commit |
|----|-------|--------|--------|
| PR13 | PromptTier + compact tool/skills/context budgets | Done | — |
| PR14 | Ledger v2 for the Small tier | Done | — |
| PR15 | Prompt-weight metrics + eval expectations | Done | — |

Plan-04 PR15 deviations:

- Schema bump shared with plan 03 PR12 (v3→v4, one bump — see the
  plan-03 notes above).
- `RunMetricsAccumulator::set_system_prompt` exists and is tested,
  but the production call at `print_mode.rs`'s accumulator
  construction is NOT yet wired (file owned by another work
  stream); exported `prompt.system_prompt_tokens` stays 0 until
  that one-line call lands.
- Eval expectations: `find_provider_trait` and
  `read_readme_highlights` tightened from `max_tokens = 80000` to
  `24000` (Small-tier level: ~4k turn-0 prompt × a handful of
  turns).

## Amendments (added 2026-06-12, field-evidence driven)

| PR | Plan | Title | Status | Commit |
|----|------|-------|--------|--------|
| PR16 | 01 | Unknown-tool rescue (name + schema fingerprint) | Done | — |
| PR17 | 01 | Unknown-prop stripping + grounded repair prompts | Done | — |
| PR18 | 03 | Near-duplicate call detector (Signal C) | Done | — |

PR18 deviation: pairs of same-tool calls whose arguments differ
only in numeric leaves (read `offset`/`limit` pagination) are
exempt from the similarity streak — the plan's Jaccard-over-string-
tokens rule scores consecutive chunks of one file as maximally
similar. Exact repeats still count. The plan-mandated pagination
negative control is `paged_read_negative_control.toml`.

The revised landing order (PR16-17 → plan 04 → plan 03 incl. PR18 →
plan 02) was executed as one working tree: PR10/11/13/14/16/17/18
and the plan-02 PRs above landed together; "—" in the Commit column
means uncommitted at last tracker update. Evidence:
field_notes/2026-06-12_qwen3.5-0.8b_session.md.
