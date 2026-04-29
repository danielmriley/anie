# RLM + context-management — execution

Tracker for the plans in this folder. Update inline as
work lands.

## Branches

| Branch | Plans landed there |
|---|---|
| `main` | Plan 01 (stagnation detection + aggressive compaction) |
| `dev_rlm` | Plan 02 (RLM `recurse` tool) |
| TBD | Plans 03, 04, 05 (deferred / contingent) |

## Status

| Plan | Branch | Status | Commit |
|---|---|---|---|
| [01 — Stagnation detection + aggressive compaction](../01_stagnation_detection.md) | `main` | landed | `a2863e7` |
| [02 — RLM `recurse` tool (shape 1)](../02_recurse_tool.md) | `dev_rlm` | landed (Phase A of 06) | `64dcbe2` |
| [03 — RLM recurse intent (shape 2)](../03_recurse_intent.md) | TBD | deferred | — |
| [04 — Native RLM compat (shape 3)](../04_native_rlm_compat.md) | TBD | speculative | — |
| [05 — Passive context management](../05_passive_context_management.md) | TBD | absorbed into 06 | — |
| **[06 — Phased path to context virtualization](../06_phased_implementation.md)** | `dev_rlm` | **Phases A + B landed; next: Phase C** | — |
| [07 — Evaluation harness + mode flags](../07_evaluation_harness.md) | `dev_rlm` | partial (`--harness-mode` flag; scenarios deferred) | `8a162d3` |

## Phase status (Plan 06)

| Phase | Description | Status | Commits |
|-------|-------------|--------|---------|
| A | `recurse` tool | **landed** | rlm/01–06.3 (`8a162d3..64dcbe2`) |
| B | indexed external store | **landed** | rlm/07 (`ec2ccb4`) |
| C | active context ceiling + FIFO eviction | not started | — |
| D | ledger injection | not started | — |
| E | smart inclusion (relevance-based paging-in) | not started | — |
| F | background summarization for paged-out content | not started | — |

## Phase A sub-commit breakdown (rlm/01–06.3 on `dev_rlm`)

| Sub | What | Commit |
|---|---|---|
| 01 | `--harness-mode {baseline,current,rlm}` flag plumbing | `8a162d3` |
| 02 | `SubAgentFactory` + `ContextProvider` + `RecurseScope` traits/types in anie-agent | `4dc5a2e` |
| 03 | `ControllerContextProvider` with `MessageRange` resolution | `bf602e8` |
| 04 | `RecurseTool` (drives a sub-`AgentRunMachine`) | `9c04dbb` |
| 05 | Wire `RecurseTool` into the controller for `--harness-mode=rlm` | `a5503d5` |
| 06.1 | `MessageGrep` scope resolution | `32a5b70` |
| 06.2 | `ToolResult` scope resolution | `084675c` |
| 06.3 | `File` scope resolution + provider doc cleanup | `64dcbe2` |

## Phase B sub-commit breakdown (rlm/07 on `dev_rlm`)

| Sub | What | Commit |
|---|---|---|
| 07 | indexed `ExternalContext` store; `ToolResult` lookup is now O(1) | `ec2ccb4` |

## Ordering rationale

- **Plan 01 lands on main first** because it's bounded,
  immediately useful, and unblocks the user-visible
  "compaction is being skipped" problem that motivated
  this work. It's a safety-net upgrade, not a paradigm
  shift — fits cleanly into `main`.
- **Plan 02 ships on `dev_rlm`**, a fresh branch off
  `main` after 01 lands. It's a real capability addition
  with new public types (`SubAgentFactory`,
  `ContextProvider`, `RecurseScope`) and a new tool. The
  branch isolation gives us room to iterate without
  perturbing main while we measure.
- **Plans 03–05 are deferred or contingent.** Plan 03
  (intent shape) only makes sense after we have eval data
  showing shape 1's limits. Plan 04 needs a natively-
  recursive model published to a backend anie supports.
  Plan 05 is a parallel track that becomes priority-
  worthy if eval data points at within-turn context
  pressure as the bottleneck.

## Pause points

- **After Plan 01.** Confirm the stagnation detector
  catches real cases (run a long Ollama session with
  qwen3.5:9b that previously hit "budget exhausted"; verify
  aggressive compaction kicks in instead). If it doesn't
  fire when expected, fix before moving on.
- **After Plan 02.** Run the eval suite (when it lands —
  see `docs/small_model_capability_ideas_2026-04-29.md`
  Tier 3 #10) against a baseline (no `recurse`) and
  the recurse-enabled harness. The 28.3% delta from the
  paper is the ceiling; even a fraction of it justifies
  Plan 03 / 04 work.

## Reference

- Paper: [Recursive Language Models, arXiv 2512.24601](https://arxiv.org/abs/2512.24601).
- Source: [github.com/alexzhang13/rlm](https://github.com/alexzhang13/rlm).
- Companion ideas: `docs/small_model_capability_ideas_2026-04-29.md`.
- Substrate: `docs/repl_agent_loop/`, `docs/midturn_compaction_2026-04-27/`.
