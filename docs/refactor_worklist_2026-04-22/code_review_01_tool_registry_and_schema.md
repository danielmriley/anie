# Plan 01 — tool registry + schema validation

**Findings covered:** #1, #10

This is the cleanest small win in the report: anie's tool registry
is doing fixed work repeatedly at runtime even though tool metadata
never changes after registration.

## Rationale

Two review findings live on the same object:

1. **#1** — `jsonschema::validator_for()` is compiled on every tool
   call in `crates/anie-agent/src/agent_loop.rs`.
2. **#10** — `ToolRegistry::definitions()` re-sorts tools on every
   call in `crates/anie-agent/src/tool.rs`.

Both are a sign that `ToolRegistry` is being treated as a dynamic
container when it is effectively a startup-time immutable catalog.

pi is **not** ahead here: it creates a singleton AJV instance but
still calls `ajv.compile(tool.parameters)` inside validation on
every call (`pi/packages/ai/src/utils/validation.ts:64-72`). So
this plan is an anie-specific cleanup, not a pi port.

## Design

### 1. Cache the sorted `ToolDef` list in `ToolRegistry`

Today `definitions()` collects and sorts every time. The registry
already controls registration order and there is no runtime
re-registration path during a session, so the registry should own a
cached, already-sorted view:

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    sorted_definitions: Vec<ToolDef>,
    validators: HashMap<String, ValidatorState>,
}
```

On `register()`:

1. call `tool.definition()`
2. insert the tool
3. insert / update the validator state
4. rebuild `sorted_definitions`

Because registration happens only at startup, rebuilding once per
registration is fine. The runtime path becomes a simple clone of
the already-sorted vector (or a borrow if the API changes later).

### 2. Precompile validators at registration time

The validator cache should **not** silently drop invalid schemas.
If registration-time compilation fails, we still want the tool to
exist so the error is visible and explicit on use. Use an enum:

```rust
enum ValidatorState {
    Ready(Arc<jsonschema::Validator>),
    Invalid(String),
}
```

This preserves current semantics:

- a valid schema validates arguments quickly
- an invalid schema still surfaces a deterministic, explicit error

### 3. Keep call-site logic simple

`execute_single_tool` should fetch the tool and validator state from
the registry, then:

- `Ready(v)` → run `iter_errors(args)`
- `Invalid(msg)` → return the stored schema-compilation error

No runtime schema compilation remains on the hot path.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-agent/src/tool.rs` | Add cached definitions + validator state to `ToolRegistry`; update `register()` and `definitions()`. |
| `crates/anie-agent/src/agent_loop.rs` | Stop compiling validators inline; consume precompiled validator state from the registry. |
| `crates/anie-agent/src/tests.rs` and/or `agent_loop.rs` tests | Add registry/validation regression tests. |

## Phased PRs

### PR A — cache sorted definitions

1. Add `sorted_definitions: Vec<ToolDef>` to `ToolRegistry`.
2. Rebuild it in `register()`.
3. Make `definitions()` return the cached list.
4. Add a regression test that repeated `definitions()` calls do not
   depend on insertion order and return a stable sequence.

### PR B — registration-time validator compilation

1. Add `ValidatorState`.
2. Compile `def.parameters` inside `register()`.
3. Store either `Ready` or `Invalid`.
4. Replace the runtime `validator_for()` call in
   `validate_tool_arguments`.
5. Add tests for:
   - valid schema path
   - invalid schema path
   - argument validation error formatting remaining readable

### PR C — tighten API surface (optional but recommended)

1. Audit all `definitions()` callers.
2. If every caller only needs a borrow, change the API to `&[ToolDef]`
   and drop the per-call vector clone entirely.
3. If that churn is not worth it, explicitly defer and keep the
   cached-`Vec` clone as the final state.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `tool_registry_definitions_are_sorted_once_and_stable` | `crates/anie-agent/src/tool.rs` tests |
| 2 | `tool_registry_returns_cached_definitions_in_registration_order_after_sort` | same |
| 3 | `execute_single_tool_uses_precompiled_validator` | `crates/anie-agent/src/agent_loop.rs` tests |
| 4 | `invalid_tool_schema_surfaces_registration_time_error_on_use` | same |
| 5 | Existing tool-call validation tests remain green | existing agent/tool tests |

## Risks

- **Error timing changes:** schema compilation failure moves from
  first tool use to registration time. Storing `Invalid(String)`
  avoids behavior changes visible to the user.
- **Validator type ergonomics:** if `jsonschema::Validator` is not
  cheap to move, store it behind `Arc`.
- **Registry mutability assumptions:** if future extension loading
  adds tools at runtime, `register()` still keeps the cache correct.

## Exit criteria

- [ ] `ToolRegistry::definitions()` no longer sorts on the runtime
      path.
- [ ] Tool argument validation no longer compiles schemas per tool
      call.
- [ ] Invalid schemas still produce explicit, user-visible errors.
- [ ] Regression tests 1-5 pass.

## Deferred

- Zero-clone borrowing API for `definitions()` if call-site churn is
  not worth the first PR.
- Any larger "tool metadata view" refactor beyond cached
  definitions/validators.

