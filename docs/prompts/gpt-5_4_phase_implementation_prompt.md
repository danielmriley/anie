# GPT-5.4 Prompt — Implement anie-rs in Phase Order

Use the following prompt with GPT-5.4.

---

You are implementing the `anie-rs` project inside its repository.

Your job is to implement the system **phase by phase, in order**, following the project planning documents exactly.

## Primary objective

Implement the project through the planned phases in sequence, with special focus on the **v1.0 release blockers** and the **execution order** already defined in the docs.

Do **not** skip ahead. Do **not** silently redesign core architecture. Do **not** implement post-v1.0 features unless explicitly asked after the v1.0 work is complete.

## Read these docs first

Before changing any code, read these documents completely:

1. `@docs/IMPLEMENTATION_ORDER.md`
2. `@docs/v1_0_milestone_checklist.md`
3. `@docs/notes.md`
4. `@docs/anie-rs_build_doc.md`
5. `@docs/anie-rs_architecture.md`
6. every file in `@docs/phase_detail_plans/`

## Source-of-truth precedence

If the docs differ, use this precedence order:

1. `docs/IMPLEMENTATION_ORDER.md`
2. `docs/v1_0_milestone_checklist.md`
3. `docs/phase_detail_plans/`
4. `docs/anie-rs_build_doc.md`
5. `docs/anie-rs_architecture.md`
6. `docs/notes.md`

If you find a **real conflict** between the higher-priority docs and lower-priority docs, stop and ask the user before proceeding.

## Non-negotiable architectural constraints

Do not violate these:

1. **v1.0 is local-first**
   - OpenAI-compatible support is the primary provider path.
   - Ollama / LM Studio support is required for zero-cost development/testing.
   - Anthropic is strongly desired.
   - Google is optional stretch.
   - GitHub Copilot OAuth is post-v1.0.

2. **Owned context only**
   - `AgentLoop::run(...)` takes owned context.
   - It returns `AgentRunResult`.
   - Do not reintroduce shared mutable transcript ownership between TUI and agent loop.

3. **Structured provider errors only**
   - Provider streams must yield `Result<ProviderEvent, ProviderError>`.
   - Do not convert provider failures into unstructured strings in the core architecture.

4. **UI/orchestration split**
   - `anie-tui` is UI-only.
   - `anie-cli` / the interactive controller owns config, auth, sessions, compaction, and agent runs.

5. **Session persistence source of truth**
   - Persist prompts/results from the controller and `AgentRunResult`.
   - Do not base persistence on render events.

6. **Do not re-expand scope**
   - Do not silently pull post-v1.0 features into the critical path.

## How to execute

Follow `docs/IMPLEMENTATION_ORDER.md` strictly.

### Required execution style

- Work **step by step**.
- At the start of each step, say which step you are working on.
- Read the corresponding phase detail doc(s) before implementing that step.
- Implement only what is needed for that step and its gate.
- Run the relevant tests/checks for that step.
- Do not move to the next step until the current step’s gate is satisfied or you are blocked.

### Keep docs consistent

If you make a **small, non-architectural** implementation clarification that should be reflected in docs, update the relevant docs in the same pass.

Do **not** leave behind:
- stale signatures
- contradictory docs
- zombie plan sections
- obsolete references to removed designs

If the needed doc update would change a core design or scope boundary, stop and ask first.

## What counts as a critical decision

If you hit any of the following, **stop immediately and ask the user what path to take**:

1. A choice that changes a core public interface or architecture, such as:
   - provider trait shape
   - agent loop ownership model
   - session file format
   - compaction storage model
   - TUI/controller boundary

2. A choice that changes v1.0 scope, such as:
   - pulling in Copilot OAuth
   - deferring Ollama / LM Studio support
   - dropping Anthropic support from the plan
   - adding major unplanned subsystems

3. A choice between two materially different implementations where both are reasonable, such as:
   - manual stream state machine vs `async_stream`
   - session branching behavior that affects resume semantics
   - RPC protocol behavior beyond the documented v1 surface
   - a different edit-application model

4. A change that would require rewriting or invalidating multiple planning docs.

5. A blocker where the plan is not detailed enough to safely continue.

## How to ask when blocked

When you need user input, do **not** ask a vague question.

Instead, provide:
- a one-paragraph summary of the decision
- **Option A**
- **Option B**
- tradeoffs for each
- your recommendation
- the exact files/subsystems affected

Then stop and wait.

## What to do when something is not a critical decision

If the decision is small and local, make the smallest choice consistent with the docs and continue.
Examples:
- exact module split inside a crate
- helper function naming
- minor test fixture structure
- small dependency choices already implied by the plan

## Implementation priorities

Your first practical milestone should be the shortest path to a real local vertical slice:

1. workspace skeleton
2. protocol types
3. provider core contracts + mock provider
4. agent loop
5. `read` / `write` / `bash`
6. OpenAI-compatible provider
7. Ollama manual config
8. CLI harness

Do not move past that milestone until it works.

## Testing and validation expectations

At each step:
- run the smallest relevant tests first
- then run broader checks as appropriate
- fix failures before moving on

Use the phase docs and `docs/v1_0_milestone_checklist.md` as the acceptance criteria.

## Deliverable style while working

For each implementation chunk, report briefly:
- what step you completed
- what files changed
- what tests/checks passed
- whether the next step is unblocked

## Final rule

If you must choose between:
- moving quickly by inventing architecture, or
- pausing to preserve the planned design,

choose to **pause and ask**.

Start by reading the docs, summarizing the first three execution steps in your own words, and then begin with **Step 0**.