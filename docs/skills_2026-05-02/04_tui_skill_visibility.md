# PR 4 — `/skills` slash command (status-bar segment deferred)

## Rationale

PR 1 makes skills discoverable to the model (catalog in
system prompt). PR 2 makes them loadable. PR 3 ships
the initial set. PR 4 makes them visible to the **user**
— so the user can see what skills are available, which
ones the agent has loaded this run, and where each
skill came from on disk.

Without PR 4, skills work but feel invisible. The user
has no way to know whether the agent ever loaded a
skill, or which skills are even installed.

## Scope as shipped

The original plan called for two surfaces — slash
command AND status-bar segment. PR 4 ships only the
slash command; the status-bar segment is deferred. The
slash command's `Active in this run:` summary line
gives the same visibility (which skills loaded) without
the additional TUI plumbing required for a status-bar
segment (event-channel extension, status-line layout
changes, additional tests in anie-tui).

If smoke shows users want the at-a-glance status-bar
segment as well, we add it as a small follow-up PR.

## Design

Two surfaces, both small:

### A. Status-bar segment when skills are active

When `ControllerState.active_skills` is non-empty,
render a status-bar segment:

```
skills: cpp-rule-of-five, verify-after-edit
```

When the set is empty, render nothing (silent —
matches the existing pattern for `harness_mode` /
`archive` segments).

The set updates whenever the SkillTool fires; the
event channel from agent → TUI already exists for
other status updates. We just push an event when
`active_skills.write().insert(...)` returns true.

Truncation: if the set has >3 skills, render the first
2 + a count: `skills: cpp-rule-of-five, verify-after-edit, +2`.

### B. `/skills` slash command

A new slash command listing the catalog:

```
$ /skills
Available skills:
  cpp-rule-of-five           [bundled]
    When implementing a C++ class that owns raw new/delete...
  decompose-multi-constraint-task  [bundled]
    When a task involves many interacting constraints...
  use-recurse-for-archive-lookup  [bundled]
    When a follow-up question would be answered by something...
  verify-after-edit          [bundled]
    After editing or writing a file under test, re-run...

Active in this run: cpp-rule-of-five, verify-after-edit
```

Skills marked `disable_model_invocation` ARE included
in this listing (the user might want to load them via
slash command even though the model can't auto-invoke).
Mark them with a `[hidden]` suffix:

```
  internal-only             [bundled, hidden]
```

The slash command is read-only — it lists skills,
doesn't load them. Loading via slash command is
deferred (could land as `/skill load NAME` in a
follow-up).

## Files to touch

- `crates/anie-cli/src/skill_tool.rs` — emit a
  `SkillActivated` event (or extend an existing
  event) when a new skill is added to `active_skills`.
  The TUI listens on the same channel it uses for
  archive-count updates.
- `crates/anie-cli/src/commands.rs` (or wherever
  slash commands are defined) — add `/skills`
  command handler.
- `crates/anie-tui/src/status.rs` (or wherever the
  status bar is composed) — render the active-skills
  segment when non-empty.
- Tests for both.

Estimated diff: ~200 LOC of code, ~100 LOC of tests.

## Phased PRs

Single PR. The two surfaces are tightly coupled (both
read from `active_skills`).

## Test plan

- `slash_skills_lists_all_registered_with_source_labels`
  — `/skills` output includes every registered skill,
  marked with its source layer.
- `slash_skills_marks_active_skills_in_summary_line`
  — when active_skills has entries, the output ends
  with "Active in this run: ..."
- `slash_skills_includes_disabled_skills_with_hidden_marker`
  — disable_model_invocation skills appear with
  `[hidden]` suffix.
- `slash_skills_handles_empty_registry`
  — with no skills, the command outputs "No skills
  registered." rather than an empty section.
- `status_bar_renders_active_skills_segment_when_nonempty`
  — TUI test confirming the segment shows up.
- `status_bar_omits_active_skills_segment_when_empty`
  — no segment when active_skills is empty.

## Risks

- **Status-bar real estate.** Adding another segment
  competes for horizontal space. Mitigation: only
  render when active_skills is non-empty; truncate
  to first-2-plus-count for >3 skills.
- **Event channel coupling.** The TUI ↔ controller
  channel needs to handle a new event variant.
  Existing pattern (RlmStatsUpdate, etc.) is the
  template — minimal new surface.

## Exit criteria

- [ ] Status-bar segment renders when `active_skills`
      is non-empty.
- [ ] `/skills` slash command lists all registered
      skills with source labels and marks active
      ones.
- [ ] All six tests above pass.
- [ ] `cargo test --workspace` + clippy clean.
- [ ] Manual smoke: load a bundled skill via the
      tool, verify the segment appears in the TUI;
      run `/skills`, verify the listing is correct.

## Deferred

- `/skill load NAME` slash command for user-initiated
  loading. Useful for `disable_model_invocation`
  skills the model can't auto-invoke.
- Skill body preview in the listing (`/skills NAME`
  shows the body).
- "Mark a skill as project-pinned" — always-load
  this skill at session start. Would be a config
  setting, not a runtime feature.
