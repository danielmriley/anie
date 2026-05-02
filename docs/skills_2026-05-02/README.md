# Skills system for anie (2026-05-02)

A skill is a markdown file the agent can load into
context **on demand**, like a tool — but instead of
running code it brings focused guidance into the
working set when the agent decides it's relevant.

This series implements the Anthropic skills concept
adapted for anie's harness, with one specific design
goal: skills are how the model **discovers** and
**uses** the recurse / decompose / RLM facilities the
harness provides, without those facilities having to
fight for space in the always-on system prompt.

## Why skills, not prompt expansion

The 2026-05-02 prompt-framing post-mortem
(`docs/harness_mitigations_2026-05-01/` PR 3 follow-up)
showed that emphatic always-on system-prompt rules
have identity-shaping cost on small models — a "MUST
re-test" line in the base prompt made qwen3.5:9b
refuse non-coding questions. We can't keep adding
lines.

Skills decouple **availability** (the agent sees a
short index of skills) from **activation** (the agent
loads a skill's body when relevant). Activation cost
is paid only when the skill matters; the system prompt
stays narrow and identity-neutral.

This pattern is also how we want the agent to learn to
use recurse, decompose, and the embedding reranker.
The rlm augment today has prescriptive paragraphs
about "scan the ledger first / use recurse for prior
results"; the smoke shows that under context pressure
the agent doesn't reach for those tools. A skill like
`use_recurse_for_archive_lookup`, surfaced in the
catalog, gives the agent a discoverable handle. Same
for `decompose_multi_constraint_task` once that lands.

## Principles

- **Discoverability over instruction.** Skills are
  named and described in the catalog; the agent loads
  them by choosing to. Compare to prompt rules, which
  fight for cache space and frame-shift the model.
- **Skill body is loaded into context as a `<system-reminder>`
  user message.** Same channel as the per-turn ledger
  — the model treats it as injected guidance, not as
  identity. Loaded skills can be paged out by the
  context-virtualization policy under pressure (with
  Phase F summaries surfacing in their place).
- **Skills compose with tools.** A skill named
  `cpp_rule_of_five` can describe the pattern; the
  agent then uses `bash`/`edit`/`write` to apply it.
  Skills don't replace tools, they tell the agent
  *how* to use them on a particular kind of problem.
- **No global side effects.** Loading a skill changes
  the agent's working context for the current run,
  not a persistent state.
- **Discovery is layered.** Bundled skills (shipped
  with anie), user skills (`~/.anie/skills/`), and
  project skills (`<cwd>/.anie/skills/`) all merge
  into the catalog, with project > user > bundled
  precedence on name collisions.

## Skill file format

Each skill is a markdown file with frontmatter:

```markdown
---
name: cpp_rule_of_five
description: When implementing a C++ class that owns
  raw `new`/`delete` allocations, define all five
  special members (destructor, copy/move ctor, copy/move
  assign) — defaults will leak or double-free.
when_to_use: The user is asking you to write a C++
  class that uses raw memory allocation. Or you've
  noticed `= default` next to `new`/`delete` in
  existing code.
---

# Body — loaded into context when activated

Detailed guidance, examples, edge cases, references.
```

The frontmatter `description` and `when_to_use` are
what the catalog surfaces. The body is what gets
loaded on activation. Keep bodies tight — typically
under ~500 tokens — so loading multiple skills in one
run doesn't pressure the active ceiling.

## PRs in order

| PR | Doc | What | Status |
|---|---|---|---|
| 1 | [01_skill_discovery_and_registry.md](01_skill_discovery_and_registry.md) | Read skills from disk (bundled/user/project), parse frontmatter, build a `SkillRegistry`. Surface in the system prompt as a catalog (name + description, NOT body). | **shipped** (`ff0f7ac`) |
| 2 | [02_skill_tool.md](02_skill_tool.md) | A new `skill` tool: arg `name`, returns the body of the skill as a `<system-reminder>` injection. | **shipped** (this commit) |
| 3 | [03_bundled_skills.md](03_bundled_skills.md) | Initial set of bundled skills addressing the 2026-05-01/05-02 smoke findings: `use-recurse-for-archive-lookup`, `cpp-rule-of-five`, `verify-after-edit`, `decompose-multi-constraint-task`. (Names use hyphens — kebab-case enforced by `validate_skill_name`.) | **shipped** (this commit) |
| 4 | [04_tui_skill_visibility.md](04_tui_skill_visibility.md) | Status-bar segment showing currently-active skills; `/skills` slash command listing the catalog. | planned |
| 5 | [05_smoke_validation.md](05_smoke_validation.md) | Re-run the 11-turn smoke with skills enabled. Measure: does the model load `cpp_rule_of_five` autonomously when writing the DLL? Does it reach for `use_recurse_for_archive_lookup` instead of re-fetching? | planned |

PR 1 + 2 are infrastructure. PR 3 is content. PR 4 is
ergonomics. PR 5 is exit criteria.

## Exit criteria for the series

- [ ] All five PRs land on `dev_rlm` (or successor).
- [ ] `cargo test --workspace` + clippy clean.
- [ ] Smoke run shows the agent autonomously loading
      at least one skill during the 11-turn protocol
      (most likely `cpp_rule_of_five` during T1, or
      `use_recurse_for_archive_lookup` during T10/T11).
- [ ] No regression on the existing smoke comparison
      table.

## Relationship to the sub-agents plan

`docs/rlm_subagents_2026-05-01/` and this series are
**complementary**, not parallel:

- **Sub-agents** give the harness the *capability* to
  decompose problems and run focused parallel
  workers.
- **Skills** give the agent the *knowledge* of when
  to reach for that capability.

Without skills, the sub-agents work risks producing
the same failure mode we saw in T2: powerful tools
sitting unused because the model didn't know it
needed them. Without sub-agents, skills are just
better prompts. Together they're the working-memory
extension we identified as the real lever.

## Implementation order across the two series

Land skills 01-02 (infra) first, then sub-agents 01-03
(infra), then skills 03 (the bundled set including
`decompose_multi_constraint_task`), then sub-agents
04-05 (decompose + parallel recurse). Skills 04-05
and sub-agents 06 ship near the end as ergonomics +
validation.

This ordering means we can A/B test:
- Sub-agents alone (without the skill that recommends
  them) — measures harness raw capability.
- Skills alone (without sub-agents capability) —
  measures whether the model can self-direct given
  better discovery.
- Both together — the production target.

## Reference

- Anthropic's published skills concept (Claude Code
  agent skills, Claude Agent SDK skills) — the
  conceptual ancestor.
- `docs/rlm_subagents_2026-05-01/README.md` —
  complementary plan series.
- `docs/harness_mitigations_2026-05-01/` — the
  prompt-framing post-mortem motivating skills.
- 2026-05-02 diagnostic findings on small-model C++
  failure mode — see memory entry
  `project_smallmodel_cpp_failure_mode_2026-05-02.md`
  in the auto-memory store.
