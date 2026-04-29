# Making anie a top-class local small-model agent harness

This note collects ideas for making anie excel with local 8B–20B
parameter models. The core premise is that a small local model will not
usually match a frontier model by prompt wording alone. The harness must
externalize more of the cognition: context selection, planning,
verification, memory, retries, tool grounding, and progress control.

A useful target:

> Make one modest local model call behave like one step inside a strong,
> stateful, tool-backed reasoning system.

## Product goal

anie should become a local-first coding agent harness where small models
feel reliable because the harness provides:

- excellent task context;
- model-specific prompts and tool-call formats;
- structured step-by-step execution;
- verification loops using tools, tests, compilers, and diffs;
- persistent task state and semantic compaction;
- local backend tuning for Ollama, llama.cpp, vLLM, and similar runtimes;
- explicit budgets that prevent loops without imposing arbitrary short
  wall-clock timeouts.

The desired user experience is not that a 14B model magically becomes a
frontier model. It is that anie makes the model's weaknesses less
important by surrounding it with a disciplined workflow.

## Why small models need a stronger harness

Small local models tend to struggle with:

- long-horizon task tracking;
- noisy or excessive context;
- brittle tool-call formats;
- planning and editing too much at once;
- overconfident final answers;
- recovering from compiler/test errors;
- remembering project conventions across many turns;
- deciding when to inspect files instead of guessing.

These weaknesses map naturally to harness responsibilities. anie can
hold state, choose context, enforce step boundaries, run tools, verify
claims, and compact history more reliably than the model can do from raw
conversation text alone.

---

# 1. Treat local models as workers inside a strong agent runtime

Rather than asking a small model to solve a whole coding task in one
large pass, anie should turn the task into a sequence of narrow,
observable steps:

1. understand the request;
2. gather relevant context;
3. propose a small plan;
4. inspect files;
5. make one focused edit;
6. run focused checks;
7. inspect failures;
8. repair;
9. summarize only after evidence is available.

This is the central quality multiplier for local models. The harness
should make the model do less per call, but make more calls with better
state and feedback.

## Useful policy knobs

```toml
[agent.local]
mode = "structured"
max_steps = 40
max_repair_rounds = 4
max_consecutive_tool_errors = 3
require_read_before_edit = true
require_diff_review_before_final = true
require_validation_for_code_changes = true
```

These are not impatience timeouts. They are loop-quality budgets that
keep a persistent agent from repeating low-value actions forever.

---

# 2. A REPL architecture for local agents

REPL means **Read → Eval → Print → Loop**. This maps very well to a
local-model agent harness.

For anie, a local-agent REPL could look like:

```text
while task is not done:
    READ:
        collect current task state, relevant context, tool observations,
        queued user corrections, validation results, and budget state

    EVAL:
        ask the model for exactly one next action, or ask a verifier to
        critique/score candidate actions

    PRINT:
        emit the action, tool result, state change, progress message, or
        final answer to the UI/session log

    LOOP:
        update structured task state, compact if needed, adjust budgets,
        decide whether to continue, ask the user, or stop
```

## Why REPL helps small models

A REPL turns agent behavior into explicit state transitions. This helps
because local models do better when each call has:

- a clear current state;
- a narrow decision to make;
- recent observations;
- a bounded action space;
- immediate feedback from tools.

Instead of relying on the model to remember and self-correct across a
long chat transcript, the REPL runtime maintains the truth.

## REPL step shape

A future anie local-agent step could have a shape like:

```rust
struct AgentStepInput {
    task: TaskState,
    context: ContextBundle,
    observations: Vec<Observation>,
    budgets: BudgetState,
    allowed_actions: Vec<ActionKind>,
}

struct AgentStepOutput {
    rationale_summary: String,
    action: AgentAction,
    confidence: Confidence,
    needs_user_input: Option<String>,
}
```

The model should not be asked to output arbitrary prose when the runtime
needs an action. It should output one of a small set of actions:

```text
- SearchRepo(query)
- ReadFile(path, range)
- EditFile(path, patch)
- RunCommand(command)
- AskUser(question)
- UpdatePlan(plan)
- FinalAnswer(summary)
```

The harness then validates and executes the action.

## REPL and the existing anie architecture

anie already has ingredients for this:

- an agent loop;
- tool execution;
- controller/UI events;
- cancellation;
- session persistence;
- compaction work;
- active input / queued follow-up plans.

A REPL-oriented local runtime would make these boundaries more explicit:

- every model/tool interaction becomes a step;
- every step produces an observation;
- every observation updates task state;
- queued user input can be folded into the next READ phase;
- cancellation can stop at tool/model boundaries cleanly;
- progress can be shown after each PRINT phase.

## REPL design principle

Do not make the REPL a rigid artificial pause after every token. The
unit should be a meaningful agent step: one model action, one tool call,
one edit, one test run, one review, or one final response.

---

# 3. Structured task state outside the model

Small models lose track of goals and decisions. anie should hold an
explicit task state and include a compact version in every local-model
step.

Example:

```text
TaskState
- User goal: Let users type follow-up prompts while the agent runs.
- Non-negotiable constraints:
  - Ctrl+C must still abort.
  - Enter must not silently clear drafts.
  - Preserve single-run architecture.
- Current plan:
  1. Route active editing keys to InputPane.
  2. Add queued prompt action.
  3. Drain queue after current run.
- Files inspected:
  - crates/anie-tui/src/app.rs
  - crates/anie-tui/src/input.rs
  - crates/anie-cli/src/controller.rs
- Files modified:
  - crates/anie-tui/src/app.rs
- Validation:
  - active typing tests pending
- Risks:
  - active Enter can clear draft before controller accepts it
```

This is much more useful to a local model than a raw transcript.

## Candidate `TaskState` fields

- original user goal;
- latest user correction;
- accepted plan;
- open questions;
- constraints;
- files inspected;
- files modified;
- commands run;
- failing tests;
- validation status;
- next recommended step;
- decisions and rationale;
- deferred work.

---

# 4. Model capability profiles

anie should not treat all local models the same. A harness optimized for
local models needs model profiles.

Example:

```toml
[models.qwen-coder-14b-local]
provider = "ollama"
model = "qwen2.5-coder:14b"
context_window = 131072
effective_context_detection = true
prompt_template = "qwen_coder_local"
tool_call_format = "xml_json_block"
tool_call_repair = true
preferred_temperature = 0.1
supports_parallel_tools = false
max_tool_calls_per_step = 1
requires_explicit_planning = true
good_at = ["rust", "typescript", "repo_navigation"]
weak_at = ["long_horizon_planning", "ambiguous_tool_json"]
```

Useful profile dimensions:

- actual/effective context window;
- best prompt template;
- reliable tool-call format;
- support for native JSON/schema constrained output;
- good default temperature/top-p;
- whether planning should be explicit;
- whether to force one tool call per turn;
- whether to use repair loops for malformed output;
- known strengths and weaknesses.

This connects to existing anie ideas around local context detection,
compatibility flags, and provider-specific behavior.

---

# 5. Model-specific prompt templates

Local models are very sensitive to prompt style. anie should ship prompt
presets for common local coding models instead of using one universal
system prompt.

Potential presets:

- `generic_local_coder`;
- `qwen_coder`;
- `deepseek_coder`;
- `llama_coder`;
- `mistral_coder`;
- `gemma_coder`.

Each preset can tune:

- tool-call syntax;
- examples;
- verbosity;
- planning requirements;
- final answer format;
- how strongly to require file inspection;
- whether to ask for one action at a time;
- how to handle uncertainty.

## Local-model system prompt stance

For local coding models, the prompt should strongly prefer evidence:

```text
You do not know this repository until you inspect it.
Search before making claims about code locations.
Read files before editing them.
Make one focused change at a time.
Run focused validation after code changes.
Do not claim tests pass unless you ran them.
```

---

# 6. Robust local tool-call formats and repair

Tool calling is often the make-or-break issue for local models. Native
provider tool calling may be unavailable or unreliable, so anie should
support local-friendly structured formats.

Possible formats:

```xml
<tool_call>
{"name":"read","arguments":{"path":"crates/anie-tui/src/app.rs"}}
</tool_call>
```

or:

```markdown
```anie-tool
{"name":"bash","arguments":{"command":"rg \"AgentUiState\" crates/anie-tui/src"}}
```
```

The parser should be strict enough to be safe but forgiving enough to
recover from common local-model formatting mistakes.

## Repair loop

If a tool call is malformed, do not fail the whole task. Ask for a
repair:

```text
Your tool call was invalid:
- missing required field: arguments.path
Return exactly one corrected tool call and no other text.
```

Budget this repair loop:

```toml
[agent.local.tool_calls]
max_parse_repairs = 2
execute_ambiguous_calls = false
```

## Constrained decoding

Where local backends support it, anie should use constrained decoding:

- llama.cpp grammars;
- vLLM guided decoding;
- JSON schema output;
- provider-specific structured output modes.

This can dramatically improve local tool reliability.

---

# 7. Context selection: quality over quantity

Small models suffer more from noisy context than frontier models. anie
should invest in a context builder that selects, compresses, and stages
context deliberately.

## Repository map

Maintain a compact project map:

- file tree;
- crate/module relationships;
- public symbols;
- important types/functions;
- test names;
- recent edits;
- known conventions from `AGENTS.md` and docs.

Example context:

```text
Relevant project map:

crates/anie-tui/src/app.rs
- App event loop
- AgentUiState
- handle_key_event
- handle_active_key
- UiAction emission

crates/anie-cli/src/controller.rs
- InteractiveController
- current_run
- PendingRetry
- SubmitPrompt active-run rejection
```

## Tiered context

Prefer staged context over dumping entire files:

1. project map;
2. symbol/function summaries;
3. grep results;
4. focused excerpts;
5. full files only when necessary;
6. test output excerpts;
7. previous decisions.

## Context budgeter

A context budgeter should reserve space for:

- system prompt;
- task state;
- current plan;
- selected code;
- tool results;
- validation failures;
- user follow-up/corrections;
- model response.

Small models need this discipline more than large models.

---

# 8. Semantic compaction, not generic summarization

Conversation compaction for local models should preserve task structure,
not just compress prose.

Good compacted state:

```text
Current task:
- Implement queued active TUI prompts.

Decisions:
- Preserve single-run invariant.
- Queue prompts only after current run finishes.
- Ctrl+C behavior must not change.

Files touched:
- crates/anie-tui/src/app.rs
- crates/anie-cli/src/controller.rs

Important tests:
- active_streaming_accepts_text_input_without_submitting
- queued_prompt_runs_after_current_run_finishes

Known risks:
- Enter must not clear draft before queue action succeeds.
```

Bad compacted state:

```text
We discussed active input and the user wants the TUI improved. Some
plans were made. There are concerns about queueing and tests.
```

For local models, compaction should produce sections:

- goal;
- constraints;
- decisions;
- current implementation state;
- files and symbols;
- validation status;
- failed attempts;
- next step.

---

# 9. Verification and critic loops

A small model can be wrong on the first try if the harness makes it
cheap to catch and repair mistakes.

Useful roles, even with the same model:

1. **Planner** — proposes a small plan.
2. **Critic** — checks the plan for missing files, risks, and tests.
3. **Coder** — makes a focused edit.
4. **Reviewer** — reviews the diff against the task.
5. **Tester** — chooses and runs validation.
6. **Fixer** — repairs failures.

This can be configured by effort level:

```toml
[agent.local.verification]
mode = "diff_review"
critic_model = "same"
max_repair_rounds = 3
require_tests_for_code_changes = true
```

## Diff review before final answer

Before final response, ask the model or a verifier step:

```text
Review this diff against the user request and project constraints.
List only blocking issues, missing tests, or overclaims.
```

Then either repair or produce an evidence-based final answer.

---

# 10. Recursive language model techniques

Recursive language model techniques use the model repeatedly on smaller
subproblems, summaries, critiques, or branches. These techniques are
especially relevant for local models because they replace one difficult
call with many simpler calls.

The important warning: recursion must be controlled by the harness, not
left as vague "think recursively" prompting. anie should own recursion
depth, branching factor, stopping conditions, and evidence gates.

## 10.1 Recursive task decomposition

Break a large task into subgoals until each subgoal is atomic enough for
a local model.

Example:

```text
Task: Make active input queue follow-up prompts.

Subtasks:
1. Identify TUI input lock path.
2. Let active states edit draft safely.
3. Add queued prompt UI action.
4. Add controller FIFO queue.
5. Add retry interaction policy.
6. Add tests.
```

Then solve each subtask through the REPL loop. The recursive process is:

```text
solve(task):
    if task is atomic:
        execute with REPL steps
    else:
        decompose into subtasks
        solve each subtask
        integrate and verify
```

Good stopping rule: a subtask is atomic when it can be completed by one
small edit plus one focused validation command.

## 10.2 Recursive context summarization

Summarize at multiple levels:

- function summary;
- file summary;
- crate/module summary;
- project/task summary.

When the model needs detail, expand only the relevant branch.

Example:

```text
controller.rs summary
- Owns current_run and PendingRetry.
- Handles UiAction::SubmitPrompt.
- Polls ui_action_rx while a run is active.

Expand: handle_ui_action()
- SubmitPrompt while current_run.is_some() emits active-run warning.
- Abort cancels current run.
- SlashCommand dispatch has guards.
```

This lets a local model navigate large repos without ingesting everything.

## 10.3 Recursive self-refinement

Use repeated improve/check loops:

```text
Draft answer/edit → critique → revise → critique → stop when clean or
budget exhausted.
```

For coding, the critique should be evidence-grounded:

- inspect diff;
- inspect compiler/test output;
- inspect relevant code;
- compare to constraints.

Do not run unlimited self-reflection. Use budgets and stagnation checks.

## 10.4 Tree-of-thought / branch-and-score planning

For hard design choices, ask the local model for several candidate
plans, then score them.

Example:

```text
Generate 3 designs for active follow-up submission:
A. allow typing only;
B. queue after current run;
C. abort and send.

Score each on implementation risk, session correctness, UX, and test
surface. Pick the best staged plan.
```

Keep branching small:

```toml
[agent.local.recursion]
max_plan_branches = 3
max_branch_depth = 2
```

This can improve design quality without exploding runtime.

## 10.5 Least-to-most prompting

Ask the model to solve simpler precursor questions first:

1. Where is input locked?
2. What happens if Enter submits while active?
3. What controller state prevents concurrent runs?
4. What safe behavior can land first?

Then use those answers to solve the larger design.

This is very compatible with anie tools because each precursor question
can be grounded in `rg`/`read` results.

## 10.6 Recursive code review

Review from broad to narrow:

1. Does the diff match the user goal?
2. Does it preserve architecture invariants?
3. Are state transitions correct?
4. Are error/cancellation paths correct?
5. Are tests sufficient?
6. Are docs/final claims accurate?

This can be implemented as a verifier prompt over `git diff` plus the
structured task constraints.

## 10.7 Reflexion-style memory

After a failed attempt, save a concise lesson:

```text
Lesson:
- Active Enter cannot call InputPane::submit() before queue support,
  because submit clears the draft even if the controller rejects it.
Source:
- crates/anie-tui/src/input.rs submit behavior
- crates/anie-cli/src/controller.rs active SubmitPrompt rejection
```

Feed these lessons into later steps and future similar tasks.

## 10.8 Recursive tool-use plans

The model can recursively refine tool strategy:

```text
Need to edit controller queue behavior.
  Need to inspect UiAction enum.
  Need to inspect current_run lifecycle.
  Need to inspect retry state.
  Need to inspect controller tests.
```

Each need becomes a tool action. The harness tracks which information
has been gathered and prevents repeated identical searches.

## Recursion guardrails

Recursive methods can waste time or spiral. anie should enforce:

- max recursion depth;
- max branches;
- max repair rounds;
- max repeated same action;
- max consecutive invalid tool calls;
- evidence requirements before edits;
- validation requirements before final answer;
- cancellation at every model/tool boundary;
- progress-aware idle budgets rather than arbitrary short hard stops.

Example config:

```toml
[agent.local.recursion]
enabled = true
max_depth = 4
max_plan_branches = 3
max_self_refine_rounds = 2
max_repair_rounds = 4
stop_on_repeated_action = true
require_new_evidence_per_round = true
```

---

# 11. Test-driven local agency

Compilers and tests are the strongest verifier available to a local
coding agent.

A local TDD mode could:

1. identify or write a focused failing test;
2. run it;
3. inspect failure;
4. make a minimal edit;
5. rerun;
6. repeat until green or blocked.

```toml
[agent.local.tdd]
enabled = true
prefer_focused_tests = true
require_validation_before_final = true
max_repair_rounds = 4
```

This allows the local model to be imperfect while the harness uses hard
feedback to converge.

---

# 12. Evidence-based final answers

Small models often overclaim. anie should guide final answers into an
evidence-based format:

```text
Done:
- Changed active key routing in crates/anie-tui/src/app.rs.
- Added tests for active typing and Ctrl+C behavior.

Validation:
- cargo test -p anie-tui active_input passed.
- cargo fmt --all -- --check passed.

Not run:
- Full workspace clippy.
```

The harness can provide observed validation results to the final-answer
step and discourage claims not backed by tool output.

---

# 13. Memory with sources

Local model memory should be concise, source-linked, and practical.

Good memory:

```text
- anie optional persisted fields need serde(default) and
  skip_serializing_if when applicable. Source: AGENTS.md.
- Use ProviderError taxonomy instead of string-matching errors.
  Source: AGENTS.md.
- Active input must preserve Ctrl+C abort behavior. Source:
  docs/active_input_2026-04-27/README.md.
```

Avoid vague memory:

```text
- The user cares about quality.
- There was a discussion about TUI input.
```

Memory should help the model make better next actions, not simply remind
it of old conversations.

---

# 14. Local backend excellence

anie should integrate deeply with local inference backends.

## Ollama

- effective `num_ctx` detection;
- context-length override;
- keep-alive configuration;
- model-load diagnostics;
- quantization/model-size hints;
- warmup behavior;
- clear messages for memory/resource failures.

## llama.cpp

- grammar-constrained tool calls;
- JSON schema where available;
- KV cache reuse opportunities;
- rope/context settings;
- GPU layer/offload diagnostics.

## vLLM

- guided decoding;
- paged attention/long context;
- batching for verifier/critic loops;
- structured output modes;
- performance telemetry.

## Telemetry useful for local tuning

- effective context window;
- prompt tokens and completion tokens;
- context budget allocation;
- tokens/sec;
- time in model vs tools;
- tool-call parse failures;
- repair rounds;
- compaction events;
- validation commands and outcomes.

---

# 15. Local eval suite

anie needs practical evals to know what improves local model behavior.

Suggested eval categories:

## Repo navigation

- find where a feature is implemented;
- identify config loading path;
- locate relevant tests;
- explain a state machine from source.

## Editing

- add a small config field;
- fix a TUI keybinding;
- add a controller state transition;
- update tests.

## Tool use

- search before claiming code location;
- read before edit;
- avoid reading huge files unnecessarily;
- run focused validation;
- recover from compiler error.

## Long-context/task-state

- continue after compaction;
- obey previously recorded decisions;
- incorporate queued user correction.

## Model comparison

Run the same suite across known local models and profiles:

- Qwen Coder variants;
- DeepSeek Coder variants;
- Llama coding variants;
- Mistral/Codestral where appropriate;
- Gemma coding variants.

Use eval results to tune prompt templates, context selection, and
recursion policies.

---

# 16. Human-in-the-loop steering

Local agents need lightweight correction. The active-input plans are
important because they let the user steer without waiting passively.

Useful affordances:

- type a draft while the model runs;
- press Enter to queue a follow-up;
- interrupt-and-send when the agent is on the wrong path;
- ask for approval before broad edits;
- show the current plan and next action;
- expose validation status.

This lowers the cost of correcting small-model mistakes.

---

# 17. Suggested implementation roadmap

After review, the ordering should put REPL first. Model profiles,
recursive techniques, verifier loops, and context intelligence all become
cleaner once the agent runtime has explicit step boundaries. The
architecture plan for that foundation lives in
`docs/repl_agent_loop_2026-04-27.md`.

## Phase A — REPL step runtime foundation

- Add behavior-characterization tests for the current loop.
- Refactor `AgentLoop::run` into an explicit Read → Eval → Print → Loop
  shape without changing behavior.
- Define internal agent step input/output shapes.
- Make model/tool actions explicit and validated.
- Preserve live streaming/tool progress events.
- Keep controller/session policy ownership unchanged.

## Phase B — Local reliability foundation

- Add model capability/profile support.
- Add model-specific prompt templates.
- Add robust local tool-call parser and repair loop.
- Add evidence-based final answer template.
- Improve local backend diagnostics.
- Fold queued user input into future REPL READ phases once the basic
  loop is stable.

## Phase C — Context intelligence

- Build repository map/index.
- Add tiered context selection.
- Add context budgeter.
- Add source-linked memory.
- Add semantic compaction format.

## Phase D — Verification loops

- Add planner/critic/coder/reviewer modes.
- Add diff review before final answer.
- Add TDD mode.
- Add bounded repair loops.

## Phase E — Recursive techniques

- Add recursive task decomposition.
- Add recursive context summaries.
- Add bounded branch-and-score planning.
- Add reflexion-style lessons from failures.
- Add recursion budgets/stagnation detection.

## Phase F — Evals and tuning

- Build local coding-agent eval suite.
- Track per-model scorecards.
- A/B prompt templates and tool-call formats.
- Add recommended local model setup profiles.

---

# Highest-leverage first steps

If we want the quickest path to a noticeably better local-model anie,
start with:

1. **REPL-style step boundaries** — one explicit next action at a time,
   with observations fed back into state. This is the foundation for the
   rest of the list.
2. **Structured task state** — keep goals, constraints, decisions, files,
   failures, and next steps outside the model.
3. **Model profiles + prompt templates** — stop treating local models as
   generic chat models.
4. **Robust tool-call parsing/repair** — local tool use must be reliable.
5. **Context builder/repo map** — give small models the right context,
   not the most context.
6. **Diff/test verifier loops** — use tools and tests as the oracle.
7. **Recursive decomposition with budgets** — split hard tasks into
   atomic subproblems without letting recursion spiral.
8. **Local eval suite** — measure which prompts and harness policies
   actually improve small-model performance.

The strategic direction is simple: frontier models hide a lot of
reasoning inside the model. anie can make local models competitive by
moving that reasoning into an explicit, inspectable, testable agent
runtime.
