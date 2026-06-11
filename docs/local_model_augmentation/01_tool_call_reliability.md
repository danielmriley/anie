# Plan 01 — Tool-call reliability for small models

## 1. Rationale

Malformed tool calls are the dominant per-turn failure for
7–14B models. Today the pipeline is strict and unforgiving:

- Ollama's native `/api/chat` delivers tool-call arguments as a
  **pre-parsed JSON value** (`OllamaToolFunction.arguments:
  Option<serde_json::Value>`,
  `crates/anie-providers-builtin/src/ollama_chat/streaming.rs:279`),
  so string-level JSON repair is moot — the failure surface is
  **schema mismatch**, not syntax.
- Arguments are validated against the tool's compiled JSON
  schema in the agent loop (`validate_tool_arguments`,
  `crates/anie-agent/src/agent_loop.rs:2192`; validators are
  compiled at registration, `crates/anie-agent/src/tool.rs:~180`).
- On validation failure the harness synthesizes a failed
  `ToolResult` (`error_tool_result` →
  `finalize_synthetic_failure`,
  `agent_loop.rs:1636-1673,1763-1783`) and sends the schema
  error back to the model. Per
  `docs/small_model_capability_ideas_2026-04-29.md` §3, small
  models often misread this as "the tool is broken" — each
  malformed call costs ≥1 full turn.
- `ProviderError::ToolCallMalformed` is classified **terminal**
  (`crates/anie-cli/src/retry_policy.rs:133`);
  `ModelOutputMalformed` (Ollama-side parse rejection, e.g.
  truncated Qwen `<tool_call>` XML) retries at most 2×
  (`retry_policy.rs:40,209`).
- **No coercion, repair, or lenient validation exists anywhere
  in the pipeline today** (verified absence across
  `anie-providers-builtin`, `anie-agent`, `anie-provider`).

Common observed small-model mistakes are mechanically fixable:
an object schema received as a JSON-encoded *string*, numbers
or booleans as strings, a scalar where a one-element array is
expected. Fixing those in code is free; everything else gets
one focused, budget-capped repair round.

## 2. Design

Three layers, cheapest first:

### 2a. Schema-guided argument coercion (deterministic)

New module `crates/anie-agent/src/arg_coerce.rs`:

```rust
/// Attempt safe, schema-guided coercions on tool-call
/// arguments before validation. Returns the (possibly
/// rewritten) value and a list of human-readable coercion
/// notes (empty = untouched). Only coerces when the target
/// schema type is unambiguous and the conversion is lossless.
pub fn coerce_arguments(
    schema: &serde_json::Value,
    args: serde_json::Value,
) -> (serde_json::Value, Vec<String>)
```

Coercions, in order, all schema-driven:

1. Whole-args string that parses as a JSON object, where the
   schema root is `"type": "object"` → parse and replace.
2. Per-property: string `"42"` / `"4.2"` where schema says
   `integer`/`number` (lossless parse only); string
   `"true"`/`"false"` where schema says `boolean`.
3. Per-property: scalar where schema says `array` with
   matching `items` type → wrap in a one-element array.
4. Per-property: string-encoded object/array where schema says
   `object`/`array` → parse and replace.

Wired into `execute_single_tool` immediately before
`validate_tool_arguments` (`agent_loop.rs:1636-1673`). When a
coercion fired and validation then passes, append a one-line
note to the tool result `details` ("arguments coerced: …") —
same transparency convention as the edit tool's
whitespace-fuzzy counter (`crates/anie-tools/src/edit.rs:145-151`).
Counted per run for metrics (PR 4).

### 2b. Bounded repair round (generative)

Follows the `small_model_capability_ideas_2026-04-29.md` §3
sketch. When validation fails *after* coercion and repair
budget remains, instead of finalizing a synthetic failure the
loop issues a focused repair request:

- New `AgentLoopConfig` knobs:
  `with_tool_call_repair(bool)` and
  `max_repair_rounds: u32` (default 2, mirroring
  `MAX_MODEL_OUTPUT_MALFORMED_RETRIES`,
  `retry_policy.rs:40`).
- The repair request is a **side request, not a context
  mutation**: same model, a minimal message list containing
  the system prompt's tool definition for the one failing
  tool, the invalid call, the schema error, and the
  instruction "emit exactly one corrected call to
  `<tool>`; do not add commentary". The main context is
  untouched (principle 2: no debris).
- The corrected call replaces the original's arguments
  (keeping the original `id` so ToolExec events stay
  coherent); re-validated; on second failure → today's
  synthetic-failure path, with `wrap_failed_tool_results`
  applying as it does now (`agent_loop.rs:2252`).
- Default **on** when `--harness-mode=rlm`, env-disable via
  `ANIE_DISABLE_TOOL_REPAIR=1` (same gating pattern as
  `should_wrap_failed_tool_results`,
  `crates/anie-cli/src/controller.rs:2816-2820`).

anie-specific deviation from the ideas-doc sketch: the sketch
proposed a new `AgentIntent::RepairToolCall` variant routed
through `decide`. A side request keeps the intent state
machine untouched and is simpler to test; documented here as
the chosen shape.

### 2c. Per-tool example calls (prompt-side)

The tool catalog rendered into the system prompt
(`compose_system_prompt`,
`crates/anie-cli/src/controller.rs:2945`) gains one
canonical example invocation per tool, e.g.:

```
- edit: ... Example: {"path":"src/main.rs","oldText":"fn a()","newText":"fn b()"}
```

- Examples are a new optional `ToolDef.example` field
  (serialized only into the prompt catalog, never into the
  wire `tools` array — `tool_schema.rs` untouched).
- Gated to rlm mode so hosted-provider prompts are
  byte-identical to today.
- Static per tool → the system prompt stays stable across
  turns; `SystemPromptCache` behavior is unchanged
  (principle 3).

## 3. Files to touch

- `crates/anie-agent/src/arg_coerce.rs` (new)
- `crates/anie-agent/src/agent_loop.rs` (coercion call-site,
  repair round, config knobs)
- `crates/anie-agent/src/tool.rs` (`ToolDef.example`)
- `crates/anie-agent/src/lib.rs` (exports)
- `crates/anie-cli/src/controller.rs` (rlm gating, catalog
  example rendering)
- `crates/anie-tools/src/*.rs` (example strings per tool)
- `crates/anie-cli/src/run_metrics.rs` +
  `crates/anie-evals/src/lib.rs` (counters, PR 4)
- `crates/anie-evals/scenarios/` (PR 4)

## 4. Phased PRs

**PR 1 — `local_aug/PR1: schema-guided tool-argument coercion`**
`arg_coerce.rs` + wiring before validation + details note.
No behavior change when arguments already validate.

**PR 2 — `local_aug/PR2: bounded tool-call repair round`**
Side-request repair with `max_repair_rounds`, rlm-gated.

**PR 3 — `local_aug/PR3: per-tool example calls in the prompt catalog`**
`ToolDef.example` + rlm-only catalog rendering.

**PR 4 — `local_aug/PR4: tool-reliability metrics + eval scenarios`**
`RunMetrics.tool_repair { coerced, repaired, failed_after_repair }`
(bump `RUN_METRICS_SCHEMA_VERSION`), 3 tool-heavy scenarios.

## 5. Test plan

PR 1 (`crates/anie-agent`, unit):
- `string_encoded_object_args_coerce_when_schema_expects_object`
- `numeric_string_coerces_to_integer_only_when_lossless`
- `scalar_wraps_into_single_element_array_when_items_type_matches`
- `valid_arguments_pass_through_byte_identical_with_no_notes`
- `ambiguous_or_lossy_coercions_are_refused` (e.g. `"4.5"` →
  integer schema must NOT coerce)
- `coercion_note_appears_in_tool_result_details` (agent-loop
  level)

PR 2:
- `invalid_call_triggers_one_repair_request_and_succeeds_on_fix`
- `repair_budget_exhaustion_falls_back_to_synthetic_failure`
- `repair_request_does_not_mutate_main_context`
- `repaired_call_preserves_original_tool_call_id`
- `repair_disabled_by_env_reproduces_todays_failure_path`

PR 3:
- `tool_catalog_renders_example_line_in_rlm_mode_only`
- `wire_tools_array_never_contains_example_field`
- `system_prompt_with_examples_is_stable_across_turns`

PR 4:
- `run_metrics_v2_reports_coerced_and_repaired_counts`
- `older_metrics_schema_loads_with_repair_counters_defaulted`
- corpus: scenarios pass under `--modes current,rlm` with
  qwen3:8b (manual smoke per
  `.claude/skills/live-provider-smoke`).

## 6. Risks

- **Coercion masks model confusion.** A model that string-
  encodes everything never learns. Mitigation: coercions are
  counted and surfaced in details; if eval pass-rate rises but
  coercion counts stay high, that's acceptable — the goal is
  task completion, not model pedagogy.
- **Repair round burns tokens on hopeless calls.** Capped at 2
  rounds; the side request is minimal (one tool schema, not
  the full catalog).
- **Examples inflate the prompt.** ~15 tools × ~25 tokens ≈
  400 tokens, paid once per session under a stable prefix.
  Measured by PR 4 token metrics; revert per principle 4 if
  net-negative.
- **Repair on non-Ollama providers.** Gated to rlm mode, which
  is the local profile; hosted paths are untouched.

## 7. Exit criteria

- [ ] All four PRs landed, `cargo test --workspace` green,
      clippy `-D warnings` clean per PR.
- [ ] A deliberately malformed call (string-encoded object) is
      executed successfully with zero extra model turns.
- [ ] A schema-violating call (missing required field) is
      fixed by one repair round in a live qwen3:8b smoke.
- [ ] `RunMetrics` reports nonzero `coerced`/`repaired` on the
      new scenarios; pass-rate on the tool-heavy scenarios is
      ≥ baseline with repair on vs. off.
- [ ] Hosted-provider request bodies and prompts are
      byte-identical to pre-series behavior.

## 8. Deferred

- llama.cpp GBNF / grammar-constrained decoding (out-of-scope
  backend). Revisit if Ollama exposes grammar constraints for
  tool calls.
- Ollama `format` (JSON-schema structured outputs) for tool
  calls: `format` constrains assistant *content*, not the
  `tool_calls` array, and interacts poorly with mixed
  text+tool turns. Probe separately before designing around
  it.
- Retry-with-temperature on repeated `ToolCallMalformed`
  (plan 03 PR 2 covers the temperature plumbing; combining is
  a follow-up).
- Cross-model repair (route the repair request to a larger
  local model) — local-cascade follow-up series.
