# CLAUDE.md — guidance for AI assistants working on anie

anie is a Rust coding-agent harness. pi
(`/home/daniel/Projects/agents/pi`) is a TypeScript reference
implementation we study for shape and behavior. Many of anie's
features are being ported, influenced by, or cross-checked against
pi's equivalents.

This file captures lessons from the pi-comparison / pi-adoption
work (see `docs/anie_vs_pi_comparison.md`, `docs/pi_adoption_plan/`).
Read it before proposing changes that touch anything pi also
implements.

## Principles for pi-adoption work

### 1. Match pi's shape unless there's a documented reason not to

When adopting a pi feature, our data shapes and defaults should
match pi's unless we have a specific reason to deviate. Over-
engineering past pi's shape is the single most common mistake
caught in review passes on this project:

- pi's `CompactionDetails` has 2 fields. Ours should too.
- pi's `TerminalCapabilities` has 3 fields. Ours should too.
- pi defaults to `max_completion_tokens`. So should we.

Small shapes aren't laziness — they're how pi stays extensible.
Adding a field later is cheaper than unwinding a speculative
field.

### 2. Evidence-first when describing pi's behavior

Before writing plans that describe what pi does, **re-read the
specific file:line**. First-pass plans on this project have been
wrong — sometimes inverted from the truth — when written from
memory or assumption. The pattern that works:

- Grep for the specific type / function name in pi's code.
- Read the actual definition in context.
- Cite `path/to/file.ts:line` in the plan.
- Verify default values, not just field names. Defaults are
  often where behavior lives.

The comparison doc (`docs/anie_vs_pi_comparison.md`) uses this
convention; so does `docs/pi_adoption_plan/`. Continue it.

### 3. When we deviate, document it inline

anie-specific extensions are fine — sometimes pi is missing
something we need. But flag them:

```rust
// anie-specific (not in pi): a 2× cap on reported usage vs.
// the heuristic, to guard against provider bugs. Pi trusts
// totalTokens unconditionally.
```

This way the next person reading the code (or cross-checking
against pi) knows immediately whether a deviation was
intentional. Deviations-without-rationale read as bugs later.

### 4. Reuse existing anie deps before adding new ones

If we already depend on `fs4` for session locking, use `fs4` for
auth locking — not a different library with the same surface.
If `UiConfig` has a pattern for boolean flags, extend it rather
than creating a parallel `RenderingConfig`. Check before adding:

- `cargo tree -p anie-<crate>` to see what's already pulled in.
- Grep for similar patterns in sibling files.
- Cargo.toml diffs reveal accidental dep sprawl.

The comparison note on `fs4` vs `fs2` is a good example — same
capability, but consistency with existing code wins.

### 5. Watch for performance implications at integration points

Plans can describe a feature correctly at the unit level and
still regress performance at the integration point. The caught
case on this project:

- Plan 05 (markdown rendering) correctly described per-component
  caching but missed that streaming blocks bypass the cache
  entirely in `OutputPane::build_lines`. Rendering markdown in
  a streaming block would re-parse every frame.

Rule of thumb: before finalizing a design, trace the code path
from "feature is enabled" to "feature runs every frame / every
event / every tool call." If the per-call cost doesn't match the
call rate, flag it.

### 6. Look for separable refactors within bigger PRs

A feature that requires a type-signature change should
decompose into two commits: the refactor, then the feature.
Plan 06's `find_cut_point` signature change is logically part
of the split-turn feature, but structurally a 50-line refactor
that's easier to review on its own. Split before squash.

## Process for proposing pi-inspired changes

### Research step

Before writing a plan:

1. Read pi's code for the feature. Specific file, specific lines.
2. Read anie's current equivalent (or near-equivalent). Same.
3. Note the shape gap. Quote pi's definition and anie's current
   definition side by side.

### Planning step

Plans live under `docs/<topic>/` with this structure:

- `README.md` — index, principles, PR ordering, dependencies.
- `01_*.md`, `02_*.md`, ... — one plan per PR / feature area.
- `execution/README.md` — status tracker updated as PRs land.

Each individual plan follows the template:

1. **Rationale** — why this change, what evidence motivates it.
2. **Design** — the shape we're landing, with pi file:line
   references and any anie-specific deviations called out.
3. **Files to touch** — concrete list.
4. **Phased PRs** — one commit each. Each PR has its own test
   list and exit criteria.
5. **Test plan** — specific test names, not vague "test this."
6. **Risks** — known failure modes, mitigations or punts.
7. **Exit criteria** — bulleted, concrete, checkable.
8. **Deferred** — things we considered and explicitly don't do.

Templates to follow: `docs/max_tokens_handling/README.md`,
`docs/tui_responsiveness/README.md`.

### Review step

After writing a plan, re-verify it against pi's code. The first
review caught corrections on every one of seven plans. A useful
pattern: dispatch parallel agents to survey related plan
clusters, then synthesize corrections.

The review is not optional. Documented claims that are wrong
are worse than absent ones — they'll propagate into the code.

## Implementation conventions

### Commit messages

Keep the pattern established on this branch:

- `<area>/<PR#>: <short imperative summary>` for plan-driven PRs
  (e.g., `max_tokens/PR1: stop forwarding model.max_tokens`).
- Body: what changed + *why*, referencing the plan doc and
  relevant commits. Evidence-first.
- Bottom line: `Co-Authored-By: Claude Opus 4.7 (1M context)
  <noreply@anthropic.com>`.

### Tests

Per-PR tests should:

- Have names that describe the *behavior under test*, not
  the function being exercised
  (`rate_limit_without_retry_after_uses_fallback_floor_not_initial_delay`
  is a good example; `test_retry_delay` is not).
- Live in the crate closest to the logic (`anie-session`'s
  compaction tests live in `anie-session/src/lib.rs`, not in
  `anie-integration-tests`).
- Include regression tests with comments explaining the scenario
  they guard against, especially for anie-specific quirks.

### Adding fields / variants

Every new optional field:

- `#[serde(default)]` + `#[serde(skip_serializing_if =
  "Option::is_none")]` (or equivalent for non-`Option` types).
- If it lands on a persisted type (session entry, model,
  credential), bump `CURRENT_SESSION_SCHEMA_VERSION` and document
  the change in the changelog comment.
- Forward-compat test: older schema version loads cleanly with
  the field defaulted.

### Error handling

Use the typed `ProviderError` taxonomy
(`crates/anie-provider/src/error.rs`) — do not string-match
errors. When a new failure mode deserves distinct recovery, add
a variant and route it explicitly through
`RetryPolicy::decide`.

pi uses regex error classification. Don't copy that — anie's
structured approach is strictly better (see the comparison
doc's "anie does that pi doesn't").

## When is a pi-adoption feature "done"?

All of these true:

- [ ] Data shapes match pi's (or deviations documented).
- [ ] Behavior matches pi on the tested path.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.
- [ ] Manual smoke verifies the feature works end-to-end.
- [ ] Plan's exit criteria all checked.
- [ ] Any anie-specific deviations are called out in code
      comments with rationale.

## What not to copy from pi

See `docs/anie_vs_pi_comparison.md` "what's not worth copying":

- pi's 15 Api variants — OpenRouter covers most of them for us.
- pi's regex-based error classification — our structured
  `ProviderError` is strictly better.
- pi's component-local line cache — our block-local cache in
  `OutputPane` covers the same ground under ratatui's cell-level
  diff.

When in doubt about whether a pi feature is worth adopting,
check the comparison doc first. The "worth acting on" list
(`docs/pi_adoption_plan/README.md`) is the pre-filtered set.

## Pointers to key docs

- `docs/anie_vs_pi_comparison.md` — the functional gap analysis.
- `docs/pi_adoption_plan/` — seven prioritized plans with
  revision history.
- `docs/max_tokens_handling/` — a completed pi-adoption plan;
  reference for what "done" looks like.
- `docs/tui_responsiveness/` — another completed plan.
- `.claude/skills/adding-providers/SKILL.md` — guidance for
  adding new providers, updated with pi-adoption patterns.

## A note on cadence

Batched, phased PRs work well on this project. One commit per
logical change, with tests and clippy green before the next
commit. The commit history on this branch (`git log --oneline
main..HEAD`) is the record of what worked — mimic its cadence,
don't squash it.
