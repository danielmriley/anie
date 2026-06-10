# PR 3 — Initial bundled skill set

## Rationale

PRs 1 + 2 give the agent the *capability* to load
skills. PR 3 ships the first set of skills tied
directly to failure modes the 2026-05-01/05-02 smoke
runs surfaced. These are the proving ground: if the
agent loads even one of them autonomously during the
next smoke, the system is doing its job.

Each skill below is a candidate file in
`crates/anie-cli/skills/`. The list is not exhaustive
— it's the minimum-viable set targeting documented
failures.

## Skills to ship

### 1. `use_recurse_for_archive_lookup.md`

**Targets:** Baseline 2026-05-01 T7 wedge (model
re-fetched a URL it had already fetched). Throughout
T2 (model re-read dll.hpp from disk multiple times
when `recurse(message_grep, "DoublyLinkedList")` would
have been faster).

**Frontmatter:**

```yaml
name: use_recurse_for_archive_lookup
description: When a follow-up question would be
  answered by something already in the conversation
  archive (a URL fetched, command run, file read),
  use `recurse` to search the archive instead of
  re-fetching.
when_to_use: The current turn's question or task
  references information that already passed through
  a tool call earlier — particularly URLs, search
  queries, file contents, or bash command outputs.
  The ledger lists the prior tool calls.
```

**Body:** Concrete examples of the three scope kinds
(`message_grep`, `tool_result`, `summary`), with
fixture tool_call_ids and example arguments.
Particularly: how to read a `(id=...)` ledger entry
correctly (anti-cargo-cult — the most common failure
mode in baseline). 300-400 tokens.

### 2. `cpp_rule_of_five.md`

**Targets:** 2026-05-02 T2 + Diag 3 stalls. Model
defaults `~Class()` next to `new`/`delete` and can't
articulate-AND-implement consistently.

**Frontmatter:**

```yaml
name: cpp_rule_of_five
description: When implementing a C++ class that owns
  raw `new`/`delete` allocations, you must define
  destructor, copy constructor, copy assignment, move
  constructor, and move assignment. `= default` will
  leak (destructor) or double-free (copies).
when_to_use: You're writing or editing a C++ class
  that contains `new` / `delete` calls, raw owning
  pointers, or any resource that needs cleanup. Also
  applies if you're touching a class where compile
  errors mention copy/move semantics.
```

**Body:** Five-paragraph structure, one per special
member. Each paragraph: what it must do, what
`= default` would do wrong, a 5-10 line correct
implementation example. Emphasis: the destructor
must traverse and `delete` nodes; copy ctor must
deep-copy; move ctor must null source. Explicit
note: "if you write `// nodes are deleted via clear`
next to `~Class() = default;`, the comment is
aspirational but the code does not honor it." 400-500
tokens.

### 3. `verify_after_edit.md`

**Targets:** Baseline 2026-05-01 T5 (introduced
infinite recursion, never re-ran the binary). Closes
the gap PR 3 of the harness mitigations tried to
close via system prompt — moved to a skill where it
doesn't compete with identity.

**Frontmatter:**

```yaml
name: verify_after_edit
description: After editing or writing a file under
  test, re-run the most recent build/test command
  before claiming the change works. The harness
  ledger lists prior bash commands; find the
  build/test/run line and re-execute it.
when_to_use: You just used `edit` or `write` on a
  file that was previously compiled, tested, or run
  via `bash`. Also useful if a previous compile
  succeeded — re-running confirms behavior, not just
  that types match.
```

**Body:** Step-by-step: (1) recall the most recent
verification command via the ledger or recurse, (2)
re-run, (3) interpret the output, (4) only claim
success if the output matches expectations. Explicit
warning against "the file has been written; the
change should work" reasoning. 200-300 tokens.

### 4. `decompose_multi_constraint_task.md`

**Targets:** 2026-05-02 T2's coherence failure.
Becomes activatable once the sub-agents
`decompose_and_recurse` PR lands; this skill is
forward-compatible with the recurse tool today
(model can manually decompose with multiple recurse
calls).

**Frontmatter:**

```yaml
name: decompose_multi_constraint_task
description: When a task involves multiple
  interacting constraints (e.g., implementing a C++
  class with templates + iterators + raw memory + a
  driver + compile + run), break it into focused
  sub-problems and solve each with a separate
  recurse call. Don't try to hold all constraints in
  one generation.
when_to_use: You catch yourself rewriting the same
  file 3+ times because each fix introduces a new
  bug. Or the task has obviously separable phases
  (header, driver, build, test).
```

**Body:** A worked example: the DLL task decomposed
as (1) skeleton class + Node, (2) iterator type
alone, (3) friend declarations + erase(), (4)
constructors/destructor (rule of five), (5) driver,
(6) compile + run. Each step is its own recurse
call. The parent assembles. Note: today's recurse
sub-agents are tool-free; once tool inheritance
lands, the sub-agents can compile their own work
fragment in isolation. 400-500 tokens.

## Files to touch

- New: `crates/anie-cli/skills/use_recurse_for_archive_lookup.md`
- New: `crates/anie-cli/skills/cpp_rule_of_five.md`
- New: `crates/anie-cli/skills/verify_after_edit.md`
- New: `crates/anie-cli/skills/decompose_multi_constraint_task.md`
- Tests in `crates/anie-cli` confirming all four
  bundled skills load successfully and appear in the
  default catalog.

Estimated diff: ~50 LOC of Rust (test only); ~1500-
2000 tokens of skill content (markdown).

## Phased PRs

Single PR. The four skills are independent files;
they can be reviewed individually but ship together
since they share the loading infrastructure landed in
PR 1+2.

## Test plan

- `bundled_skills_present_in_default_catalog`
  — registry built with no user/project overrides
  contains all four bundled skills.
- `bundled_skill_bodies_load_under_token_threshold`
  — none exceed `ANIE_SKILL_MAX_BODY_TOKENS`.
- `bundled_skills_have_when_to_use_field`
  — all four have non-empty `when_to_use`.
- Smoke check: a one-shot `--print "what skills are
  available?"` lists all four.

## Risks

- **Skill content quality.** Vague guidance won't be
  loaded; over-specific guidance won't transfer.
  Mitigation: each skill targets a *specific
  documented failure*, with a concrete example. The
  smoke (PR 5 of this series) measures whether
  loading actually changes behavior.
- **Skill content drift.** Skills are checked-in
  files; over time they go stale. Mitigation: each
  skill's frontmatter includes a `targets` field
  (informal) referencing the failure mode it was
  written for. When that failure stops appearing,
  re-evaluate the skill.
- **Naming collisions with user/project skills.**
  Bundled skill names should be specific enough
  ("use_recurse_for_archive_lookup", not "recurse")
  to minimize collision. Mitigation: the registry
  precedence (project > user > bundled) lets users
  override; document this clearly.

## Exit criteria

- [ ] All four skill `.md` files exist with valid
      frontmatter.
- [ ] `bundled_skills_present_in_default_catalog`
      passes.
- [ ] Each body under the token threshold.
- [ ] `cargo test --workspace` + clippy clean.
- [ ] Manual smoke: agent in rlm mode sees all four
      in the catalog.

## Deferred

- More skills. Add only when smoke surfaces a new
  failure mode that warrants one. Resist the
  temptation to pre-write skills for every coding
  pattern.
- A test-driven skill development workflow (write a
  smoke that fails, then write a skill that fixes
  it, prove the delta). Worth doing eventually but
  not blocking PR 3.
- User-contributed skill catalog / sharing. Out of
  scope for this series.
