# 08 — Tool edit resource caps

## Rationale

The edit tool already enforces important correctness properties:
`oldText` must match uniquely, fuzzy matches must be unique, and matched
edits cannot overlap. The remaining issue is resource usage. The schema
accepts an arbitrary-length `edits` array, and runtime parsing collects
the entire list before applying limits.

Tool calls originate from model output. A malformed or adversarial tool
call with a huge edit batch can allocate substantial memory and spend a
long time matching before it fails.

Codex does not appear to fully solve this either, so this is an anie
hardening task rather than a port.

## Design

Add explicit schema and runtime caps:

- maximum edits per invocation
- maximum `oldText` bytes per edit
- maximum `newText` bytes per edit
- maximum combined edit argument bytes
- maximum input file size for `edit`
- optional maximum output file size after replacements

Proposed initial defaults:

| Limit | Initial value | Rationale |
|---|---:|---|
| edits per call | 100 | Enough for broad mechanical edits, small enough to bound matching. |
| `oldText` bytes | 64 KiB | Large hunks are still possible without accepting megabyte needles. |
| `newText` bytes | 256 KiB | Allows generated code blocks but prevents giant file writes through edit. |
| total edit arg bytes | 1 MiB | Clear upper bound on model-provided arguments. |
| input file size | 5 MiB | Avoid expensive full-file fuzzy matching on large blobs. |
| output file size | 6 MiB | Prevent accidental large expansion. |

Tune these if existing tests or real workflows show they are too tight.

## Files to touch

- `crates/anie-tools/src/edit.rs`
  - Add constants for limits.
  - Add JSON schema `maxItems` and string length hints.
  - Enforce runtime limits in `parse_edits` and before/after reading the
    file.
- `crates/anie-tools/src/tests.rs`
  - Add tests for each limit.

## Phased PRs

### PR A — Argument caps

**Change:**

- Enforce edit count and text byte limits during parsing.
- Return clear `ToolError::ExecutionFailed` messages naming the limit.
- Add schema hints matching runtime limits.

**Tests:**

- Too many edits are rejected before matching.
- Oversized `oldText` is rejected.
- Oversized `newText` is rejected.
- Combined argument budget is enforced.

**Exit criteria:**

- Model-provided edit arguments are bounded before expensive matching.

### PR B — File-size and output-size caps

**Change:**

- Reject files larger than the configured edit input cap.
- After applying edits in memory, reject outputs larger than the output
  cap before writing.

**Tests:**

- Oversized input file is rejected.
- Expansion beyond output cap is rejected and original file remains
  unchanged.

**Exit criteria:**

- Edit cannot be used as an unbounded memory/CPU amplifier.

## Test plan

- `cargo test -p anie-tools edit`
- `cargo test -p anie-tools`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke: run normal small and medium edit batches to ensure the
  caps do not block intended workflows.

## Risks

- Too-low limits can frustrate legitimate generated refactors. Keep
  errors actionable: "split this into smaller edit calls."
- Schema limits are advisory to the model; runtime checks are the real
  protection.
- Ensure rejected oversized output does not partially write the target.

## Exit criteria

- Edit resource usage is bounded by named constants and tests.
- Existing correctness checks for unique/non-overlapping edits remain.

