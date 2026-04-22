# code_review_performance_2026-04-21 parallel workstreams

This document reorganizes the existing performance-cleanup plans into
**agent-owned workstreams** so multiple implementation agents can work
at the same time without repeatedly colliding in the same files.

Scope: this covers the current plan set under
`docs/code_review_performance_2026-04-21/`. The modularity review is
not included here because it does not yet have its own numbered
implementation plans.

## How to use this

1. Assign work by **PR slice**, not by entire plan folder.
2. Keep one agent as the owner of each **hotspot file family**.
3. Do not let two agents touch the same hotspot family in the same wave.
4. Let optional tail PRs land only after the primary lane for that file
   family is quiet.

In practice, the safe unit of parallelism here is usually **one plan
lane per file cluster**, not one lane per crate.

## Hotspot ownership rules

| Hotspot file family | Plans / PRs | Rule |
|---|---|---|
| `crates/anie-tui/src/output.rs`, `app.rs`, `tests.rs`, `markdown/layout.rs` | 04A-F, 09A-C, 10A-D, 08D | **Single UI owner.** Do not split these across agents. |
| `crates/anie-agent/src/agent_loop.rs` and nearby agent wiring | 01B-C, 02A-F | **Single agent/runtime owner.** |
| `crates/anie-session/src/lib.rs` | 03A-F, 08B | **Single session owner.** |
| `crates/anie-providers-builtin/src/**` | 06A-G, part of 08A (`openai/convert.rs`) | Prefer **single provider owner** until Plan 06 is quiet. |
| `crates/anie-tools/src/**` | 07A-H | **Single tools owner.** |

The plans below are organized around those rules.

## Recommended workstreams

### Workstream A — UI core lane

**Owner:** one UI-focused agent  
**Primary files:** `output.rs`, `app.rs`, `tests.rs`, `markdown/layout.rs`

**Sequence**

1. 04A-04F — TUI output hot path
2. 09A-09C — tool output display modes
3. 10A-10C — scrollbar + markdown overflow
4. 08D — remaining small TUI/CLI helper allocations
5. 10D — optional horizontal overflow follow-up

**Why this stays single-owner**

Plans 04, 09, and 10 all reopen the same TUI hub files, especially
`output.rs`, `app.rs`, and `anie-tui/src/tests.rs`. Splitting them
between agents would create conflict churn even if the features are
conceptually different.

**Do not parallelize with**

- any other plan slice that edits `app.rs`
- any other plan slice that edits `output.rs`
- any other plan slice that edits `anie-tui/src/tests.rs`

---

### Workstream B — agent/runtime lane

**Owner:** one agent/runtime-focused agent  
**Primary files:** `anie-agent/src/tool.rs`, `anie-agent/src/agent_loop.rs`

**Sequence**

1. 01A-01C — tool registry + schema validation
2. 02A-02E — agent turn ownership + event payload cleanup
3. 02F — optional `AgentEnd` payload follow-up

**Why this stays single-owner**

Plan 01 starts in `tool.rs`, but its meaningful hot-path work ends up in
`agent_loop.rs`, which is also the center of Plan 02. Keeping the lane
single-owned avoids back-to-back rebases in the main agent loop.

**Notes**

- If 01C is deferred, continue directly to 02A.
- Treat 02F as a tail item because it may widen to protocol, CLI, and TUI
  consumers.

---

### Workstream C — session lane

**Owner:** one session-focused agent  
**Primary files:** `anie-session/src/lib.rs`

**Sequence**

1. 03A-03E — session indexing + context construction
2. 03F — session-local helper sweep
3. 08B — token-estimation helper cleanup, if still not absorbed earlier

**Why this stays single-owner**

Plan 03 is intentionally split into small PRs, but they all still land in
the same monolithic session file. Running them in parallel would trade one
large refactor for repeated merge conflicts in `lib.rs`.

---

### Workstream D — search/picker lane

**Owner:** one TUI-search-focused agent  
**Primary files:** `widgets/fuzzy.rs`, `overlays/model_picker.rs`,
`autocomplete/command.rs`, `widgets/text_field.rs`

**Sequence**

1. 05A-05D — picker search + fuzzy matching

**Why this can run in parallel**

This lane mostly stays out of the central TUI hub files. It is the safest
TUI-adjacent workstream to run alongside the UI core lane.

**Watch for**

- If implementation starts pulling model-picker lifecycle code back into
  `app.rs`, stop and re-home that work into Workstream A instead.

---

### Workstream E — provider lane

**Owner:** one provider-focused agent  
**Primary files:** `anthropic.rs`, `openai/streaming.rs`,
`openai/tagged_reasoning.rs`, `model_discovery.rs`, `local.rs`, `util.rs`

**Sequence**

1. 06A-06F — provider streaming + local models
2. 06G — provider helper sweep

**Why this can run in parallel**

This lane is structurally isolated from the session, tools, and main TUI
hotspots.

**Notes**

- Keep 06A isolated and well-tested; it is the correctness-sensitive
  Anthropic change.
- Do not start 08A until this lane is done with its provider-crate work,
  because 08A reopens `crates/anie-providers-builtin/src/openai/convert.rs`.

---

### Workstream F — tools lane

**Owner:** one tools-focused agent  
**Primary files:** `read.rs`, `grep.rs`, `bash.rs`, `edit.rs`,
shared truncation helper module

**Sequence**

1. 07A-07G — tool read/grep/bash/edit + truncation
2. 07H — optional streamed read follow-up

**Why this can run in parallel**

The tools crate already has a clean one-file-per-tool layout, so this is
the easiest large plan to parallelize safely.

---

### Workstream G — misc CLI cleanup lane

**Owner:** one general cleanup agent, only after the hotter lanes calm down  
**Primary files:** `compaction.rs`, `print_mode.rs`, `model_catalog.rs`,
possibly `openai/convert.rs`

**Sequence**

1. 08A — text assembly helper sweep
2. 08C — model-catalog helper cleanup

**Why this waits**

Plan 08 is supposed to mop up after the hotter plans stabilize. PR 08A also
crosses multiple surfaces (`openai/convert.rs`, `compaction.rs`,
`print_mode.rs`), so it is a poor fit for the first wave.

**Do not start until**

- Workstream E is done with `openai/convert.rs`-adjacent provider work
- Workstream B is done with any `print_mode.rs` fallout from 02F

## Recommended waves

### Wave 1 — maximum safe parallelism

Run up to **6 agents** at once:

1. Workstream A: 04A-04F
2. Workstream B: 01A-01C
3. Workstream C: 03A-03E
4. Workstream D: 05A-05D
5. Workstream E: 06A-06F
6. Workstream F: 07A-07G

This gives broad parallel progress while keeping each hotspot file family
single-owned.

### Wave 2 — dependent follow-through

After Wave 1 settles:

1. Workstream A continues with 09A-09C, then 10A-10C
2. Workstream B continues with 02A-02E
3. Workstream C finishes 03F, then 08B if still needed
4. Workstream E finishes 06G if still needed
5. Workstream F finishes 07H if approved
6. Workstream G starts 08A and 08C only after the overlap rules above are satisfied

### Wave 3 — tail / optional cleanup

Land only after the main lanes are quiet:

1. 02F — optional `AgentEnd` payload change
2. 08D — remaining TUI/CLI helper allocations
3. 10D — optional horizontal overflow follow-up

## Quick assignment sheet

| Agent | Workstream | Start with | Avoid touching |
|---|---|---|---|
| Agent 1 | UI core | 04A | `agent_loop.rs`, `lib.rs`, provider/tool files are fine; **no second UI agent** |
| Agent 2 | agent/runtime | 01A | `app.rs`, `output.rs`, `session/lib.rs` |
| Agent 3 | session | 03A | `agent_loop.rs`, `app.rs`, `output.rs` |
| Agent 4 | search/picker | 05A | `app.rs`, `output.rs`, `session/lib.rs` |
| Agent 5 | providers | 06A | `app.rs`, `output.rs`, `session/lib.rs`, tools crate |
| Agent 6 | tools | 07A | `app.rs`, `output.rs`, `session/lib.rs`, provider files |

## Handoff rule

When giving work to agents, hand them **specific PR slices**, for example:

- "Plan 07 PR B — grep direct-write path"
- "Plan 03 PR A — remove `id_set`"
- "Plan 04 PR D — `wrap_plain_text` rewrite"

Do **not** hand out "Plan 07" or "Plan 04" wholesale unless one agent is
explicitly owning that entire lane end to end.
