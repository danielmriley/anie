# 05 — Tool output caps that scale with context window

## Rationale

Current tool output caps are absolute byte/line constants. Examples:

- Built-in `read` tool: line/byte caps live in
  `crates/anie-tools/src/read.rs` (search for `MAX_*`).
- Built-in `bash` tool: stdout/stderr caps in
  `crates/anie-tools/src/bash.rs`.
- Web tools: `max_bytes` on `FetchOptions`
  (`crates/anie-tools-web/src/read/fetch.rs:281-298`), default
  10 MiB.

A 64-KB bash output is fine for a 200K-context Sonnet but
disproportionate for an 8K-context local model — it consumes ~75 %
of the model's window in a single tool result. Mid-turn
compaction (plan 04) helps, but it should not be the only line of
defense; ideally the tool itself returns a more conservative
amount when the configured context is small, *then* mid-turn
compaction handles the case where even that overflows.

## Design

### Effective per-tool budget formula

Define a workspace-shared helper:

```rust
pub fn effective_tool_output_budget(
    context_window: u64,
    base_default: u64,
) -> u64 {
    // Cap any single tool result at ~10 % of the configured
    // context window. For a 200K-context model, that's 20K bytes
    // — well under the existing 64K default — so we still pick
    // the user-tunable `base_default` as the upper bound.
    let context_share = (context_window / 10).max(1024);
    context_share.min(base_default)
}
```

Properties:

- For a 200K window: `(200_000 / 10) = 20_000`, less than typical
  tool defaults (e.g. 64K bash output). The effective budget is
  20K. **Behavior change for cloud users.** This is intentional:
  a single tool result claiming 30 % of the model's context is a
  smell regardless of model size, and the existing
  `tool_output_mode = "compact"` already aims at this. We pick
  the smaller of the two so cloud users see modest tightening.
- For a 64K window: `64_000 / 10 = 6_400`. Tool returns at most
  6.4K of output regardless of base default.
- For an 8K window: `8000 / 10 = 800`, floored to 1024. Tools
  return at most 1 KB.
- The 10 % factor is configurable via
  `[tools] context_share_for_output` (default 0.1) for users who
  want different sizing.

### Where it applies

Three call sites on first landing:

1. **Built-in `bash` tool** (`crates/anie-tools/src/bash.rs`):
   when truncating stdout/stderr.
2. **Built-in `read` tool** (`crates/anie-tools/src/read.rs`):
   when applying the byte cap to a streaming read.
3. **Web `read` tool**
   (`crates/anie-tools-web/src/read/fetch.rs`): the existing
   `FetchOptions::max_bytes` continues to be the absolute cap;
   the per-call effective budget is the smaller of `max_bytes`
   and `effective_tool_output_budget(window, max_bytes)`.

Other tools (web `search`, future tools) follow the same pattern
as they're added.

### Where the context window comes from

Tools today receive `(call_id, args, cancel, update_tx)` per the
`Tool::execute` trait
(`crates/anie-agent/src/tool.rs:25-37`). There is **no
existing `ToolExecutionContext` struct** — adding context
metadata requires either a trait-signature change or a new
optional bag of state. Reviewed options:

**A. Breaking trait change.** Add a new arg to `Tool::execute`,
e.g. `ctx: ToolExecutionContext` carrying `context_window` (and
likely future per-execution metadata). Touches all 9 current
implementations: `bash`, `read`, `write`, `find`, `ls`, `grep`,
`edit` in `anie-tools`, plus `web_search` and `web_read` in
`anie-tools-web`. Mostly mechanical, but it's a workspace-wide
churn.

**B. New trait method with default impl.** Add
`async fn execute_with_context(&self, ctx: &ToolExecutionContext,
...)` with a default impl that delegates to `execute`. Tools that
want to be context-aware override the new method. Non-breaking
for existing tools, opt-in for new behavior.

**C. Side-channel via setter on the registry.** Tools learn
context-window via a global `Arc<AtomicU64>` set by the agent
loop before each `execute`. Ugly, no new struct needed, but
sneaky and races on parallel tool execution.

**Recommend A.** The plumbing change is contained (one new arg,
9 mechanical edits), and the resulting struct is the right
extension point for future per-execution state ("estimated
remaining-budget tokens after this call," "is this a follow-up
inside a turn that has already compacted," etc.). B looks
cleaner short-term but produces two parallel APIs that drift.

### `ToolExecutionContext` shape (new)

```rust
/// Per-execution context handed to every Tool::execute call.
/// Carries metadata the agent loop knows about the current
/// invocation; tools can ignore fields they don't care about.
#[derive(Debug, Clone)]
pub struct ToolExecutionContext {
    /// Effective context window for the current model
    /// (post-`/context-length`-override). Tools use this to
    /// scale output budgets via `effective_tool_output_budget`.
    pub context_window: u64,
}
```

Lives in `crates/anie-agent/src/tool.rs` next to the `Tool` trait.

## Files to touch

- `crates/anie-agent/src/tool.rs`
  - Define `ToolExecutionContext` struct.
  - Add a `ctx: &ToolExecutionContext` parameter to
    `Tool::execute`. Update the trait signature.
- `crates/anie-agent/src/agent_loop.rs`
  - When dispatching tools (in the per-tool-call loop body),
    construct a `ToolExecutionContext { context_window: ... }`
    using `model.context_window` clamped against any active
    override (`AgentLoopConfig::ollama_num_ctx_override`).
- All 9 existing tool implementations:
  `crates/anie-tools/src/{bash,read,write,find,ls,grep,edit}.rs`
  and `crates/anie-tools-web/src/{search/tool,read/tool}.rs`.
  Each adds the `ctx: &ToolExecutionContext` arg. Tools that
  don't yet use it ignore it (binding `_ctx`).
- `crates/anie-tools/src/bash.rs`
  - Apply `effective_tool_output_budget(ctx.context_window,
    config.max_bytes)` to stdout/stderr truncation.
- `crates/anie-tools/src/read.rs`
  - Apply same to the read byte cap.
- `crates/anie-tools-web/src/read/fetch.rs`
  - Apply same to the fetch byte cap (called from
    `WebReadTool::execute` once the ctx is plumbed in).
- `crates/anie-config/src/lib.rs`
  - Add `context_share_for_output: f32` to
    `ToolsConfig` (default 0.1, range 0.01..=1.0).

## Phased PRs

### PR A — Introduce `ToolExecutionContext`; thread through `Tool::execute`

**Change:**

- Define `ToolExecutionContext` next to `Tool::execute`.
- Update the trait signature to take `ctx: &ToolExecutionContext`.
- Update all 9 tool impls to accept the new arg (most bind it to
  `_ctx` and ignore for now).
- Agent loop constructs the ctx for each tool dispatch.

**Tests:**

- All existing tool tests pass once their `execute` invocations
  are updated to pass an explicit `ToolExecutionContext::default()`
  or a fixture builder.
- New unit test on a stub tool that captures the context and
  asserts `context_window` is propagated correctly from the
  agent loop.

**Exit criteria:**

- Tools have access to `context_window`; no behavioral change yet.
- Workspace tests + clippy clean.

### PR B — Apply effective budget to `bash` tool

**Change:**

- `effective_tool_output_budget` helper.
- `bash` truncates against the effective budget.

**Tests:**

- `bash_truncates_stdout_to_effective_budget_for_small_window`
- `bash_uses_full_max_bytes_for_large_window`
- `bash_respects_explicit_context_share_config`

**Exit criteria:**

- Bash output on an 8K-context model is no larger than ~1KB by
  default.

### PR C — Apply to `read` (built-in)

Same shape as PR B; targets `crates/anie-tools/src/read.rs`.

### PR D — Apply to `web_read`

Same shape; targets
`crates/anie-tools-web/src/read/fetch.rs::fetch_html`. Note that
web `max_bytes` is already 10 MiB; the effective budget will
typically dominate for any window under 100M tokens. Document
this in the plan's exit criteria so it's clear we're tightening
behavior here.

## Test plan

Beyond the per-PR lists:

- Integration test: small-context (8K) Ollama model + bash tool
  invocation that produces 100KB of stdout. Assert tool result
  body is ≤1KB and that the agent loop continues normally.
- Cloud-model regression test (Anthropic-style mock): 200K
  window, bash 64KB stdout. Assert effective budget is 20K (10 %
  of 200K), not the original 64K. Document this as the user-
  visible change for cloud users.

## Risks

- **Workspace-wide trait churn.** Plumbing the new
  `ToolExecutionContext` arg through `Tool::execute` touches all
  9 current tool implementations, plus every test that
  constructs a tool invocation. Mostly mechanical; well-named
  test-helper builders keep the per-test edits one-liners. PR A
  is the LOC-heavy commit; the rest of this plan's PRs are
  small.
- **Behavior change for cloud users.** The 10 % factor tightens
  defaults for large-window models too. If this is undesired,
  raise the `context_share_for_output` default to a higher value
  (e.g. 0.3) before landing PR B. Recommend leaving the default
  at 0.1 — a single tool result taking 30 % of context is bad
  hygiene regardless of size, and the existing
  `tool_output_mode = "compact"` already shrinks tool display
  representations independently.
- **Surprising small outputs on small models.** Users running a
  4K-context model will see tools return ~400 bytes of output by
  default. Document clearly in the config comment that this is
  intentional and adjustable.
- **Tools that ignore `context_window`.** Anything that doesn't
  adopt the effective budget continues to use its hardcoded
  default. Mitigation: track adoption per tool in the execution
  tracker and surface remaining tools as follow-up plans.

## Exit criteria

- [ ] `effective_tool_output_budget` helper in shared crate.
- [ ] `ToolExecutionContext::context_window` populated by the
      agent loop.
- [ ] `bash`, built-in `read`, and `web_read` honor the effective
      budget.
- [ ] Integration tests for small + large windows.
- [ ] `cargo test --workspace`, clippy clean.

## Deferred

- **Tool output paging.** A more sophisticated path returns a
  truncated chunk with a "next" token the agent can request more
  pages of. Useful but separate concern.
- **Per-tool budget overrides.** Some tools may want fixed
  budgets regardless of context. Out of scope here; can be added
  via a `[tools.bash] budget_override` style escape hatch later.
- **Streaming tool output adaptation** — surfacing a "would have
  truncated; here's the first 1KB" placeholder in real time.
  Speculative.
