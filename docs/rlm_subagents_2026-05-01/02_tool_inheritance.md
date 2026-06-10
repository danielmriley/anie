# PR 2 — Sub-agents inherit a filtered tool registry

## Rationale

Today, sub-agents spawned by `RecurseTool` get an
**empty** tool registry. Look at
`crates/anie-cli/src/recurse_factory.rs:67-72`:

```rust
Arc::new(ToolRegistry::new()),  // empty
```

The comment says *"sub-agents in this commit are non-
recursive and tool-free."* That was right for the
initial recurse landing — sub-agents only needed to
read context scopes. But it's the gating constraint
on every interesting use of recurse:

- A sub-agent solving a sub-problem can't `bash` to
  verify its answer.
- A sub-agent looking up live data can't `web_read`.
- A sub-agent fixing a bug can't `edit` the file.

Without tools, sub-agents are pattern-matchers on the
parent's archive. With tools, they become actual
specialist agents — the unlock for the rest of this
plan series.

## Design

Replace the empty registry with a **filtered** copy of
the parent's registry. Filter rules:

- **Always inherit:** `bash`, `read`, `edit`, `write`,
  `web_search`, `web_read`. These are self-contained
  side-effecting tools; sub-agents using them
  independently is fine.
- **Conditionally inherit `recurse`:** only when
  `ctx.depth < ANIE_RECURSE_DEPTH_LIMIT_FOR_INHERITANCE`
  (default `3`). Beyond that depth, sub-agents see no
  `recurse` — they have to solve their problem with
  the other tools or terminate without recursing.
- **Never inherit:** any tool registered by the
  parent that's marked `parent_only` (a new flag on
  `Tool::definition()`). Reserved for future tools that
  shouldn't be sub-agent accessible (e.g., a hypothetical
  `commit_to_parent_archive` tool).

The depth check is a soft cap on `recurse` only — it's
NOT a hard cap on overall recursion (PR 1's principle
holds). A sub-agent at depth 5 can still bash and edit;
it just can't spawn another sub-agent.

## Files to touch

- `crates/anie-tools/src/lib.rs` (or per-tool defs) —
  add `parent_only: bool` to `ToolDef` (default false).
- `crates/anie-cli/src/recurse_factory.rs` — replace
  `Arc::new(ToolRegistry::new())` with a filtered
  clone of `state.tool_registry`. Take depth into
  account.
- `crates/anie-agent/src/tool.rs` — `ToolRegistry`
  gains a `filtered(predicate)` method.
- Tests at all three levels.

Estimated diff: ~200 LOC of code, ~120 LOC of tests.

## Phased PRs

This is a single PR. Could split into:
- 2.1 — `ToolRegistry::filtered`.
- 2.2 — `parent_only` flag.
- 2.3 — Wire filtered inheritance in `recurse_factory`.

But the three are tightly coupled; one PR is fine.

## Test plan

- `tool_registry_filtered_returns_predicate_subset`
  — unit test on the new method.
- `tool_definition_default_parent_only_is_false`
  — backward-compat assertion.
- `sub_agent_inherits_bash_and_web_read_at_depth_zero`
  — construct factory, build sub-agent, verify its
  registry contains the inherited tools.
- `sub_agent_excludes_recurse_when_depth_at_limit`
  — at `depth = ANIE_RECURSE_DEPTH_LIMIT_FOR_INHERITANCE`,
  recurse is filtered out.
- `sub_agent_includes_recurse_when_depth_below_limit`
  — at `depth - 1`, recurse is included.
- Integration test: drive a 2-deep recurse chain
  through a MockProvider where the inner sub-agent
  calls bash (mocked) and verify the result flows back.

## Risks

- **Tool side-effects from sub-agents.** A sub-agent
  edit/write/bash modifies the user's filesystem
  without the parent's explicit consent. Mitigation
  (this PR): no extra guard — same trust model as the
  parent. Document clearly. Future PR could add a
  read-only sub-agent mode.
- **Cost explosion.** Each sub-agent may now do real
  work, multiplying token + tool-call cost. PR 3
  (resource observability) lands the budget tracking
  to make this visible.
- **`recurse` filter logic surprises.** If a developer
  expects deep recursion to work, the soft cap will
  surprise. Mitigation: log clearly when recurse is
  filtered out. The sub-agent's tool list is reported
  in its system prompt; the model can see what it has.

## Exit criteria

- [ ] `ToolRegistry::filtered(predicate)` exists and
      is tested.
- [ ] `ToolDef::parent_only` defaults to false.
- [ ] `recurse_factory` builds sub-agents with the
      filtered registry.
- [ ] `ANIE_RECURSE_DEPTH_LIMIT_FOR_INHERITANCE` env
      var respected; default 3.
- [ ] All six tests above pass.
- [ ] `cargo test --workspace` + clippy clean.
- [ ] Smoke check: a recurse call at depth 0
      successfully runs `bash` from the sub-agent and
      returns a result the parent uses.

## Deferred

- Read-only sub-agent mode (no edit/write/bash).
  Useful for safety-conscious deployments. Add only
  when there's a real demand.
- `parent_only` actual usage. Today no tool needs it;
  the flag is plumbed for future need (and to prove
  the filter works).
- Cross-sub-agent coordination (sub-agent A writes a
  file that sub-agent B reads). The current design
  treats sub-agents as independent; cross-coordination
  goes through the parent's archive.
