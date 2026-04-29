# Boosting small-model capability in the anie harness

Companion to `docs/local_small_model_harness_ideas.md` (the
broad vision) and `docs/repl_agent_loop_2026-04-27.md` (the
just-shipped refactor). This note is the tactical follow-up:
what becomes immediately tractable now that the REPL machine,
the `BeforeModelPolicy` seam, and the soft web-truncation
landed — and what to build next, in priority order.

Target model: 8–14B local coders (qwen3.5:9b is the working
example, but the principles apply to qwen3:8b, deepseek-coder
6.7b, gemma3 12b, llama 3.3 8b, mistral nemo 12b).

## Core observation

Small local models are not "frontier model minus skill"; they
fail in specific, harness-addressable ways. The four most
common failure modes seen in real anie sessions:

1. **They forget the goal across many turns.** Mid-task they
   pivot to subgoals that aren't load-bearing.
2. **Tool-call JSON is fragile.** A misplaced quote or a
   missing required field wastes a turn and confuses recovery.
3. **They drown in irrelevant context.** A 30k-line repo map
   buried in 80k of stale chat history isn't navigable.
4. **They overclaim.** "Tests pass" without having run them;
   "I read the file" without having read it.

Every idea below maps to one of those four failure modes. The
REPL refactor gave us the right shape to address them at the
harness level — most of these ideas plug into either
`BeforeModelPolicy` (Read-phase mutation) or a future intent
variant (new step kinds in the loop) without touching the
provider/tool contract.

---

## Tier 1 — high leverage, ships in a week

These are concrete plans that fit the existing
`AgentRunMachine` shape. Each one is a single PR sized to
ship without a multi-week vision document.

### 1. Repo map injection via `BeforeModelPolicy`

**Failure mode addressed:** drowning in irrelevant context.

A `BeforeModelPolicy` implementation that, on the first
`ModelTurn` of a run, injects a compact repo map: workspace
crates + their public top-level items, recently-edited paths
from `git log -10`, the contents of `AGENTS.md` /
`CLAUDE.md`. Subsequent turns see it via the persisted
context.

**Wiring:**

```rust
struct RepoMapPolicy { /* lazy-built map, refreshed on cwd change */ }

#[async_trait]
impl BeforeModelPolicy for RepoMapPolicy {
    async fn before_model(
        &self,
        request: BeforeModelRequest<'_>,
    ) -> BeforeModelResponse {
        if request.step_index != 0 { return BeforeModelResponse::Continue; }
        let map = self.build_for(request.context).await;
        BeforeModelResponse::AppendMessages(vec![system_note(map)])
    }
}
```

**Why it's a Tier 1 win:** the seam is already shipped. One
new struct, one config wire-up, no protocol change.

**Cost guard:** the map should fit in ~2–4k tokens. Token
estimation via `anie-session` is already there.

### 2. Per-model prompt templates

**Failure mode addressed:** small models are sensitive to
prompt style; the same generic prompt that works on Claude
underspecifies what qwen3.5 needs.

`AgentLoopConfig` already has `system_prompt: String`. Add a
`PromptTemplate` enum keyed off `Model.provider` /
`Model.id` patterns:

- `generic_local` — current default, neutral coder framing.
- `qwen_coder` — qwen3.5 family. Stronger directive on tool-
  before-claim, explicit "use JSON for tools" guidance.
- `deepseek_coder` — deepseek family. Different reasoning
  style, more terse.
- `gemma_coder` — gemma family. Less reasoning-friendly,
  needs more scaffolding.
- `llama_coder` — llama 3.x. Different turn-taking patterns.

**Wiring:** `build_system_prompt` in `anie-cli` already takes
the model. Add a switch on `model.provider` / `model.id`
prefix; load the template from a static map. No protocol
change.

**Cost guard:** templates are static strings; A/B comparison
goes through the eval suite (Tier 3).

### 3. Tool-call repair loop

**Failure mode addressed:** brittle tool-call JSON.

Today: a malformed tool call (e.g., missing required field)
fails JSON-Schema validation and the agent emits a
`ToolResult { is_error: true, ... }` with the schema-error
text. The model often misreads this as "the tool is broken"
rather than "I sent invalid arguments." On qwen3.5:9b this
costs about 1 turn per malformed call.

Add a new intent: `AgentIntent::RepairToolCall { failed_call,
schema_error }`. Decide returns it instead of `ExecuteTools`
when validation fails AND we haven't exceeded
`max_repair_rounds: 2`. Eval calls the model with a focused
prompt: "your tool call was invalid; here is the schema and
what failed; emit exactly one corrected call."

```rust
enum AgentIntent {
    // ... existing variants ...
    RepairToolCall {
        failed_call: ToolCall,
        schema_error: String,
        attempts_remaining: u32,
    },
}
```

**Why it's high leverage:** the validator already produces a
typed error message; we have the schema; the model just needs
to be told "fix it" with budget.

**Cost guard:** `max_repair_rounds = 2`. After two failures
the loop falls back to today's behavior (treat as a regular
tool error).

### 4. Evidence-based final-answer system prompt

**Failure mode addressed:** overclaiming.

Append a short directive to the system prompt for any run
where `tool_registry.has("bash")`:

```
You do not know things you have not verified. Before claiming
that tests pass, you must have run them in this session.
Before claiming a file's contents, you must have read it in
this session. If you cannot verify a claim, say so explicitly
rather than inferring.
```

**Wiring:** `build_system_prompt` again. Three lines.

**Why it's a win for small models:** they need explicit
permission to say "I don't know." Without the directive they
default to confident-sounding plausible answers.

### 5. Tracing-based "stagnation detector"

**Failure mode addressed:** models that loop on the same
intent without progress.

The REPL `agent_repl_step` span already records `intent` and
`run_step`. A small middleware that watches the span stream
(or a state-side counter on `AgentRunState`) can detect:

- 3+ consecutive `ModelTurn → ExecuteTools` with the same
  tool + same args.
- 5+ consecutive turns without any state change in
  `generated_messages.len()` or `context.len()`.

When stagnation fires: emit a `SystemMessage` event nudging
the user, optionally fold a stagnation note into the next
`BeforeModelPolicy` call so the model sees "you've been
stuck on this for N turns; consider asking for help or
reframing."

**Cost guard:** purely observational; no behavior change
unless the operator opts in.

---

## Tier 2 — medium leverage, requires new intent kinds

These need new `AgentIntent` variants. The REPL machine is
shaped to make this clean: each new variant gets its own
`Eval` arm, its own `Print` arm, and Decide routes to it.
None of these touch the provider/tool contract.

### 6. Verifier intent: `VerifyEdit`

**Failure mode addressed:** overclaiming about edits +
forgetting to run validation.

After an `ExecuteTools` step that included `write` or `edit`
calls, Decide returns `VerifyEdit { paths }` instead of
`RunCompactionGate`. Eval runs:

- `cargo check` (or the language-appropriate type-checker)
  scoped to the changed files.
- The closest test to each changed file (heuristic: same
  module, then nearest test under `tests/`).

The verifier's output becomes a `ToolResult`-shaped
observation the model sees on the next `ModelTurn`. The
model then either reports done (with verified evidence) or
iterates.

**Wiring:**

```rust
enum AgentIntent {
    // ...
    VerifyEdit { paths: Vec<PathBuf>, max_runs: u32 },
}
```

The "scoped check" is a project-specific concern — anie
already has a `bash` tool, so the verifier intent can be
implemented as: "run this command, capture output, fold into
context as a synthesized observation." The interesting work
is the heuristic that picks the *right* command per language.

**Cost guard:** `max_verify_iterations = 2`. After two
failed verifies, surface the failures and let the model
decide.

### 7. Test-driven loop intent: `TddCycle`

**Failure mode addressed:** weak long-horizon planning;
overclaiming.

Decide returns `TddCycle` when the user prompt looks like a
feature request and the model emits a "let me write a test
first" plan. The cycle:

1. Write the test (model emits `write` tool call).
2. Run it (verifier runs the focused test command).
3. If failing: model edits implementation.
4. Re-run.
5. Repeat until green or `max_repair_rounds` exhausted.

The whole cycle is one logical agent turn from the user's
perspective even though it's many REPL iterations under the
hood.

**Why it's a small-model win:** small models *plan well in
small atomic steps*. TDD's red-green-refactor is exactly
that loop. The harness owns the loop semantics so the model
never has to track "am I in red or green right now."

**Cost guard:** opt-in via config flag; off by default.

### 8. Critic intent: same-model self-review

**Failure mode addressed:** small-model first attempts
underweight constraints.

After a `ModelTurn` whose stop reason is non-error and the
context has a non-trivial diff (e.g., one or more `write`/
`edit` calls in the just-finished turn), Decide can route
through a `Critic` intent:

```rust
enum AgentIntent {
    // ...
    Critic { diff: String, focus: CriticFocus },
}

enum CriticFocus {
    SecurityAndSafety,
    ConventionsFromAgentsMd,
    TestCoverage,
}
```

Eval runs the same model with a critic prompt: "review this
diff against [focus criteria]. List only blocking issues; if
none, say PASS." The critic's output is folded as a system-
note into the next `ModelTurn` so the coder model can revise
*before* the user sees the result.

**Cost guard:** budget is one critic call per user prompt
unless the policy is configured otherwise; can be disabled.

### 9. Reflexion-style failure memory

**Failure mode addressed:** repeating mistakes within a
session.

When a verifier or critic step finds a problem, write a
single-line "lesson" to a per-session file
(`~/.anie/sessions/<id>.lessons.md`). On every subsequent
`ModelTurn`, `BeforeModelPolicy` injects the file's contents
as a "things you've already learned this session" note.

```
Lessons from this session:
- The Cargo workspace requires `--all-features` for the web crate; without it `wasm-bindgen` is missing.
- `ToolRegistry::register` rebuilds the sorted def list; tests asserting unsorted order will break.
- Tracing spans must be entered with `.instrument()` not `.enter()` across `.await`.
```

This pattern beats the model passively re-reading prior turns
because each lesson is one line, denormalized, action-
shaped.

**Wiring:** combines `BeforeModelPolicy` (read) + a small
`AfterToolCallHook` extension (write).

---

## Tier 3 — structural; multi-PR investments

These need real planning documents. Listing them here so the
shape is on record but not committing to scope.

### 10. Local-model eval suite

`anie-evals` crate that drives anie's `--print` mode against
a fixed set of prompts (repo navigation, small edits, tool
use, recovery from compiler error, long-context follow-up).
Prompts and rubrics live in version control. Output: a
per-model scorecard JSON the operator can diff across runs.

This is the foundation for everything else in this list. We
can't tune what we don't measure. Without it, every Tier 1
or Tier 2 idea ships on vibes; with it, the same ideas ship
with evidence.

Suggested initial scenarios (mirror the smokes we ran during
the REPL refactor):

- "What is 2+2?" — sanity, no tools.
- "List the files here." — single-tool roundtrip.
- "Write the weather for today in NYC using web tools." —
  multi-tool, error-recovery (truncation, robots.txt).
- "Add a config field `max_widgets: u32` defaulting to 8 to
  the relevant struct." — small edit + verify.
- "Cancel during long generation." — Ctrl+C path.
- "Continue from this 50-message session and address the
  TODO in the last assistant turn." — long-context.

### 11. Constrained decoding via Ollama grammars

Ollama supports JSON-mode and Ollama's OpenAI-compatible
endpoint supports response-format hints. For tool calls we
can lock the model into emitting valid JSON for the current
tool's schema rather than free-form text.

Two ways:

- Per-call: when the model is about to emit a tool call
  (mid-stream), constrain the output to the tool's JSON
  schema. Requires hooking into provider streaming.
- Per-turn: structure the entire output as
  `{thinking, tool_calls, text}`. Simpler but less fine-
  grained.

This is provider-specific work — it lands in
`anie-providers-builtin` Ollama paths, not in `anie-agent`.
But it pairs with idea #3 (repair loop) as belt-and-
suspenders: even if the grammar fails, the repair loop
catches it.

### 12. Tiered context retrieval

`BeforeModelPolicy` decides what context to inject based on
what the *current request* needs:

- Layer 0: always-on (system prompt, repo map summary).
- Layer 1: keyword-relevant files (cheap grep against the
  user's prompt).
- Layer 2: symbol-relevant snippets (tree-sitter / grep
  with stricter scoping).
- Layer 3: full files only on explicit need.

Token budget per layer; layer 0 always fits; later layers
trim to fit. The shape lives behind a `ContextRetriever`
trait that `BeforeModelPolicy` calls.

This is the most ambitious of the three Tier 3 ideas. It
also has the highest payoff per turn for small models.

---

## Bonus: things specific to qwen3.5:9b

Observed in real sessions during this branch:

- **Tool-call format is mostly reliable** but occasionally
  emits `arguments` as a string instead of an object on
  the first attempt. Tool-call repair (Tier 1, idea #3)
  catches this exact case.
- **Reasoning blocks are long** — qwen3.5 emits 200–500
  tokens of `<think>` per turn. That's fine in 262k
  context but inflates session token cost. A
  `BeforeModelPolicy` could truncate prior reasoning
  blocks during replay (anie already has the sanitizer
  for this; just needs an aggressive replay-mode option
  for local sessions).
- **Strong at code reading, weak at long planning** — leans
  into idea #6 (TDD cycle) and idea #8 (critic) more than
  larger models would benefit from.
- **Native Ollama provider gives `num_ctx` control** — the
  config already exposes `OllamaChatApi` and
  `num_ctx_override`. For qwen3.5:9b, setting
  `num_ctx=131072` keeps generation fast while still
  fitting the working set.

---

## What NOT to build (yet)

- **Full multi-agent / agent-of-agents.** Tempting, but
  Tier 1 + Tier 2 covers the easy wins; multi-agent
  scaffolding pays off only after we have the eval suite
  to measure whether it actually helps.
- **Custom fine-tuned policies / classifier nets.** Out of
  scope; we don't ship training infrastructure.
- **A "task planner" model separate from the coder.** The
  same model wearing multiple hats (planner, critic, coder)
  works fine and avoids the latency of two model loads on
  a local machine. Revisit only if evals show planning
  quality is the bottleneck.
- **Recursive task decomposition with deep recursion.** The
  vision doc hypothesizes this; in practice, depth >2 is
  where models lose the plot. Cap at depth 1 or 2 and
  prefer flat fan-out.

---

## Suggested ordering

If we ship one Tier 1 idea per week, four weeks gets us:

| Week | Idea | Effort | Risk |
|---|---|---|---|
| 1 | #4 evidence-based prompt directive | trivial | none |
| 1 | #2 per-model prompt templates | small | low |
| 2 | #1 repo-map `BeforeModelPolicy` | medium | low |
| 2 | #3 tool-call repair loop | medium | low |
| 3 | #5 stagnation detector | small | none |
| 3 | #6 verifier intent (`cargo check`) | medium | medium |
| 4 | #10 minimal eval suite (5 scenarios) | medium | none |
| 4 | start measuring; pick Tier 2 winners | — | — |

After week 4 the eval suite tells us which Tier 2 ideas
actually move the needle; we ship those, defer the rest.

The REPL refactor was the foundation. These ideas are the
first batch of capability work that the foundation enables.
