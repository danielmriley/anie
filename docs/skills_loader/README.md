# Skills loader (SKILL.md → `/skill:name`)

Plan for a **thin** skills loader: discover `SKILL.md` files under
`~/.anie/skills/` and project `.anie/skills/`, parse their YAML
frontmatter (`name`, `description`, `allowed-tools`), register each
as a `/skill:name` slash command, and — when invoked — inject the
skill body as context for the next turn.

This is a deliberate **subset** of the deferred Plan-10 extension
system. The full out-of-process JSON-RPC subprocess extension host
(`docs/refactor_plans/10_extension_system_pi_port.md`) is **explicitly
deferred** — see [§8 Deferred](#8-deferred). The rival analysis
(`docs/rival_analysis_2026-06-06/README.md`) ranks the bespoke Plan-10
system below an MCP client for the same value at far lower cost; this
plan ships the one piece of Plan-10 that stands alone and needs no
subprocess transport.

Ranked initiative #9 in `docs/rival_analysis_2026-06-06/README.md`
("Skills loader (SKILL.md → slash command) — Thin subset of Plan 10").

---

## 1. Rationale

### The gap (verified)

`docs/rival_analysis_2026-06-06/findings_by_lens.json`, lens
`extensions-skills-hooks`, findings **EXT-2** and **EXT-4**:

- **EXT-2** — "Skills system not implemented; `SlashCommandSource::Skill`
  is a stub." The variant `SlashCommandSource::Skill { skill_name: String }`
  is defined at `crates/anie-tui/src/commands.rs:35` but **never
  constructed** in production code. No loader, no `SKILL.md` parser, no
  dispatch handler. Verified: grep finds the variant only in `label()`
  and `grouped_by_source()` display logic, zero construction sites.
- **EXT-4** — "Slash-command registry does not load external commands
  or skills." `CommandRegistry` (`crates/anie-cli/src/commands.rs:52-112`)
  is hardcoded to `with_builtins()` → `builtin_commands()`. The
  `register()` method is `#[cfg(test)]` (`crates/anie-cli/src/commands.rs:167`),
  so there is no runtime path to append a skill command. The registry is
  built once at `crates/anie-cli/src/bootstrap.rs:109` and never mutated.

The scaffolding to close the gap already exists and was added in
anticipation of exactly this work:

- `SlashCommandSource::Skill { skill_name }` variant
  (`crates/anie-tui/src/commands.rs:35`) with a `label()` arm
  (`commands.rs:46`).
- `SourceKey::Skill` grouping + the `"Skills"` `/help` heading
  (`crates/anie-cli/src/commands.rs`, `group_heading`).
- `grouped_by_source()` already iterates a `source_order` array that
  includes the `Skill` source (`crates/anie-cli/src/commands.rs`).

The comment at `crates/anie-tui/src/commands.rs:23-25` states the
variants "will be constructed once extensions (plan 10) and
prompt/skill loaders land." This plan is the skill-loader half of that
promise, with **no dependency on Plan 10**.

### Why this is worth doing now

Skills are a low-cost, high-leverage power-user feature: repeatable,
project-scoped agent behaviors checked into a repo's `.anie/skills/`
with zero code. Rivals ship it — Claude Code (pi) loads the Agent
Skills standard (`https://agentskills.io/`) from `~/.pi/.../skills/` and
registers `/skill:name` commands (per EXT-2's `rival_baseline`; the pi
reference tree is **not present on this machine**, so this plan cannot
cite live pi `file:line` — see the tooling caveat in
`docs/rival_analysis_2026-06-06/README.md`). The spec we target is
already written up in `docs/notes/skills_system.md`.

> Note on rival baselines: EXT-2/EXT-3/EXT-6 mark the Codex and Grok
> Build behaviors as `SPECULATIVE`. This plan treats those as
> hypotheses only and grounds its scope on the **confirmed** anie-side
> gap (EXT-2/EXT-4) plus the in-repo spec
> (`docs/notes/skills_system.md`), not on assumed rival internals.

---

## 2. Design

### Shape we land

Four moving parts, all in existing crates — **no new crate**, in
contrast to Plan 10's `anie-extensions`:

1. **`SkillSet` + `Skill`** (new module `crates/anie-cli/src/skills.rs`)
   — discovery, parse, and an owned map `name → Skill`. A `Skill`
   holds `name`, `description`, `allowed_tools: Vec<String>`, and
   `body: String` (everything after the frontmatter).

2. **`SlashCommandInfo` owned-string refactor**
   (`crates/anie-tui/src/commands.rs`) — today `name`, `summary`, and
   `argument_hint` are `&'static str`. Skill names are runtime values,
   so these fields must accept owned strings. We move them to
   `Cow<'static, str>` so builtins stay zero-alloc (`Cow::Borrowed`,
   const-constructible) and skills use `Cow::Owned`. This is a
   **separable refactor** (per `CLAUDE.md` principle 6) landed before
   any skill behavior.

3. **Registry runtime registration**
   (`crates/anie-cli/src/commands.rs`) — un-gate `register()` from
   `#[cfg(test)]` to `pub(crate)` (it already dedups by name, "first
   registration wins, matching pi's behavior") and add a
   `CommandRegistry::with_builtins_and_skills(skills: &SkillSet)`
   convenience that registers a `SlashCommandInfo` with
   `source: SlashCommandSource::Skill { skill_name }` for each skill.
   Wire it at `crates/anie-cli/src/bootstrap.rs:109`.

4. **Dispatch + injection** — the TUI's `dispatch_validated_command`
   (`crates/anie-tui/src/app.rs:1311`) gains a branch keyed on
   `info.source` (not on the dynamic name string): when the source is
   `SlashCommandSource::Skill { skill_name }`, it sends a new
   `UiAction::ActivateSkill(String)` carrying the skill name. The
   controller resolves the body from its `SkillSet` and **stages** it;
   the next `start_prompt_run` (`crates/anie-cli/src/controller.rs`,
   `prompt_message` construction at `controller.rs:1020`) prepends the
   staged skill body as a synthetic `Message::User` before the user's
   prompt.

### The injection path (what we build on, and why)

The scope names `external_context` / `BeforeModelPolicy` as candidate
injection seams. After reading both, we build on the **session-append**
seam, not `BeforeModelPolicy`, for evidence-grounded reasons:

- `BeforeModelPolicy` is a **single-slot** install:
  `AgentLoopConfig::with_before_model_policy` replaces one
  `Arc<dyn BeforeModelPolicy>` field
  (`crates/anie-agent/src/agent_loop.rs:415`, default
  `NoopBeforeModelPolicy` at `agent_loop.rs:406`). That slot is
  **already occupied** by `ContextVirtualizationPolicy`
  (`crates/anie-cli/src/context_virt.rs:468`, installed via the
  recurse factory). Injecting skills through the same seam would force
  policy *composition* — real work, and over-engineered for a thin
  subset.
- `ExternalContext` (`crates/anie-cli/src/external_context.rs`) is a
  `pub(crate)` archive for the recurse tool's virtualization, keyed by
  message kind/tool — not a general "inject this text next turn" API.

The seam that *is* a natural fit: skill activation stages a body in the
controller, and `start_prompt_run` already builds a `Message::User`
from the prompt text (`controller.rs:1020`) and appends it to the
session via `session.inner_mut().append_message(&prompt_message)`
before building context with `context_without_entry`. We append one
extra synthetic `Message::User` immediately ahead of the prompt:

```rust
// Staged skill bodies become a synthetic user turn injected
// ahead of the next prompt. Reuses the existing append_message
// + context_without_entry path (controller.rs:1020); does NOT
// touch the single BeforeModelPolicy slot (already held by
// ContextVirtualizationPolicy, context_virt.rs:468).
let skill_block = format!(
    "<skill name=\"{name}\">\n{body}\n</skill>",
);
let injected = Message::User(UserMessage {
    content: vec![ContentBlock::Text { text: skill_block }],
    timestamp: now_millis(),
});
```

This reaches the model verbatim (it is an ordinary user message),
persists in the session log like any other turn, survives compaction
through the existing machinery, and adds **zero** new persisted fields.

> anie-specific deviation vs. pi: pi (per EXT-2) supports auto-loading
> skills into *initial* context and tool-restriction via `allowed-tools`.
> We inject on **explicit `/skill:name`** invocation only, for the
> next turn. Auto-injection and hard tool-restriction are deferred
> (§8) — flagged here so a future reader doesn't mistake the narrower
> scope for an oversight.

### Frontmatter parsing — no new dependency

The `SKILL.md` frontmatter is a fenced `---` YAML block. The fields we
need are flat: `name: str`, `description: str`,
`allowed-tools: [a, b, c]`. There is **no YAML parser in the anie
workspace today** (verified: only `toml`/`toml_edit` are pulled in,
`Cargo.toml:79-80`; `serde_yaml` is absent and is itself
archived/unmaintained upstream). Rather than add an unmaintained YAML
crate for three flat fields, we **hand-roll a minimal frontmatter
parser** restricted to:

- a leading `---` / trailing `---` fence (first block only),
- `key: value` lines (value trimmed, optional surrounding quotes
  stripped),
- `allowed-tools` as an inline flow list `[a, b, c]` **or** a YAML
  block list (`- a` lines).

Unknown keys (`license`, `compatibility`, `metadata`,
`disable-model-invocation`) are ignored for now. The supported subset
is documented in the module header and enforced by tests. **Cargo-tree
check is a required PR step** (§4) to confirm no transitive YAML
dependency sneaks in. If review prefers spec-complete YAML over the
hand-rolled subset, the fallback is a single workspace dep line
`serde_yml = "0.0.12"` (maintained `serde_yaml` fork) — justified and
named, but **not** the default recommendation.

### Errors

A malformed `SKILL.md` (missing fence, missing required `name`/
`description`, duplicate name, unreadable file) must **not** abort
startup. Discovery is best-effort: each failure is collected into a
typed `SkillLoadError` (new, in `skills.rs`) and surfaced as a startup
warning via the existing `tracing::warn!` + system-message path; the
remaining skills still load. No `unwrap`/`panic` on user files. This
is a loader-local error type (file IO + parse), not a provider or tool
failure, so it does **not** belong in `ProviderError`/`ToolError`; those
taxonomies stay reserved for their domains per `CLAUDE.md`. (If a
future PR enforces `allowed-tools` at tool-call time, that rejection
*would* route through `ToolError`.)

---

## 3. Files to touch

| File | Change |
|---|---|
| `crates/anie-tui/src/commands.rs` | `SlashCommandInfo.{name,summary,argument_hint}` → `Cow<'static, str>`; adjust `builtin`/`builtin_with_args` ctors + `validate`. (PR 1) |
| `crates/anie-cli/src/commands.rs` | Update `lookup`/`register`/`format_help` for `Cow`; un-gate `register`; add `with_builtins_and_skills`. (PR 1, PR 3) |
| `crates/anie-cli/src/skills.rs` | **New.** `Skill`, `SkillSet`, `discover_skills`, `parse_skill`, `SkillLoadError`. (PR 2) |
| `crates/anie-cli/src/lib.rs` | `mod skills;`. (PR 2) |
| `crates/anie-cli/src/bootstrap.rs` | Discover skills; build `SkillSet`; `with_builtins_and_skills`; store `SkillSet` in `ControllerState`. (PR 3) |
| `crates/anie-cli/src/controller.rs` | `ControllerState.skills: SkillSet`; `pending_skill_injections`; handle `UiAction::ActivateSkill`; prepend in `start_prompt_run`. (PR 4) |
| `crates/anie-tui/src/app.rs` | `UiAction::ActivateSkill(String)`; dispatch branch on `SlashCommandSource::Skill`. (PR 4) |
| `docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md` | Doc updates (mark item 10 landed, describe loader). (PR 4) |

Each PR stays ≤5 files.

---

## 4. Phased PRs

Validation gate applied to **every** PR before it lands:
`cargo test --workspace`; `cargo clippy --workspace --all-targets -- -D
warnings`; `cargo fmt --check`; manual smoke per
`docs/smoke_protocol_2026-05-01.md`. Commit style
`skills/<PR#>: <imperative>` + why-body + `Co-Authored-By` line.

### PR 1 — `skills/1: make SlashCommandInfo hold owned-or-static strings`

Pure refactor, no behavior change. Move `name`, `summary`,
`argument_hint` to `Cow<'static, str>`. Keep `builtin`/
`builtin_with_args` const where possible (`Cow::Borrowed`). Fix all
`info.name == x` comparisons and `format_help` formatting. No new
public behavior.

Files: `crates/anie-tui/src/commands.rs`, `crates/anie-cli/src/commands.rs`.

Tests:
- `builtin_ctor_yields_borrowed_cow_name`
- `slash_command_info_equality_unchanged_after_cow_migration`
- existing `with_builtins_populates_known_commands`,
  `registry_covers_every_dispatched_slash_command`,
  `format_help_renders_argument_hint_column` stay green unmodified in
  intent.

Exit: workspace green; zero behavior change; `git diff` touches only
type plumbing.

### PR 2 — `skills/2: SKILL.md discovery + frontmatter parser`

Add `crates/anie-cli/src/skills.rs` with `Skill`, `SkillSet`,
`discover_skills(global_dir, project_dir)`, `parse_skill(path)`,
`SkillLoadError`. Project skills override global skills of the same
name (project-precedence, per `docs/notes/skills_system.md`). No wiring
into the registry yet — unit-testable in isolation.

Files: `crates/anie-cli/src/skills.rs`, `crates/anie-cli/src/lib.rs`.

Tests (all from temp-dir fixtures):
- `parses_minimal_frontmatter_name_and_description`
- `parses_allowed_tools_inline_flow_list`
- `parses_allowed_tools_block_list`
- `body_excludes_frontmatter_fence`
- `missing_name_is_skill_load_error_not_panic`
- `missing_closing_fence_is_skill_load_error`
- `project_skill_shadows_global_skill_of_same_name`
- `unknown_frontmatter_keys_are_ignored`
- `discovery_skips_dirs_without_skill_md`
- `one_malformed_skill_does_not_drop_the_others`

Exit: parser handles the documented subset; every failure mode returns
a typed error, never panics.

### PR 3 — `skills/3: register discovered skills as /skill:name commands`

Un-gate `CommandRegistry::register` to runtime `pub(crate)`; add
`with_builtins_and_skills(&SkillSet)`. In `bootstrap.rs`, resolve the
two skill dirs (`anie_config::anie_home().join("skills")` and the
project `.anie/skills` via the `find_project_config`-style upward walk,
`crates/anie-config/src/lib.rs:629`), discover, build the `SkillSet`,
register commands, and store the `SkillSet` on `ControllerState`. No
dispatch yet — `/help` shows the Skills group; invoking a skill hits
the existing "no handler" arm (acceptable interim, covered by PR 4).

Files: `crates/anie-cli/src/commands.rs`, `crates/anie-cli/src/bootstrap.rs`,
`crates/anie-cli/src/controller.rs` (add `skills` field only).

Tests:
- `register_runtime_skill_command_is_looked_up_by_name`
- `duplicate_skill_name_is_rejected_first_wins`
- `skill_command_groups_under_skills_heading_in_help`
- `with_builtins_and_skills_registers_one_command_per_skill`
- `empty_skill_set_leaves_registry_builtin_only`

Exit: `cargo tree -p anie-cli` shows **no new YAML/frontmatter dep**;
`/help` renders a `Skills:` section when skills are present.

### PR 4 — `skills/4: dispatch /skill:name and inject body for next turn`

Add `UiAction::ActivateSkill(String)`. In `dispatch_validated_command`,
branch on `matches!(info.source, SlashCommandSource::Skill { .. })` and
send `ActivateSkill(skill_name.clone())` plus a system-message
confirmation. In the controller, handle `ActivateSkill` by pushing the
resolved skill body onto `pending_skill_injections`; in
`start_prompt_run`, drain that buffer into synthetic `Message::User`
turns appended just before the prompt message. Update arch doc +
ROADMAP item 10.

Files: `crates/anie-tui/src/app.rs`, `crates/anie-cli/src/controller.rs`,
`docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md`.

Tests:
- `skill_source_command_dispatches_activate_skill_action`
- `activate_skill_stages_body_without_starting_a_run`
- `staged_skill_body_is_prepended_to_next_prompt_turn`
- `activating_two_skills_injects_both_in_order_once`
- `pending_skill_buffer_is_cleared_after_injection`
- `activating_unknown_skill_name_surfaces_error_not_panic`

Exit: end-to-end smoke — `/skill:<name>` then a prompt; the skill body
reaches the model exactly once; buffer empties.

---

## 5. Test plan

Names above describe **behavior under test** (per `CLAUDE.md`). Key
regression guards, with the scenario each protects:

- `one_malformed_skill_does_not_drop_the_others` — a single broken
  `SKILL.md` must not abort discovery or startup.
- `project_skill_shadows_global_skill_of_same_name` — precedence order
  matches `docs/notes/skills_system.md`.
- `duplicate_skill_name_is_rejected_first_wins` — matches the existing
  `register()` dedup contract ("first registration wins, matching pi").
- `staged_skill_body_is_prepended_to_next_prompt_turn` — the injection
  lands on the session-append seam, ahead of the user prompt, exactly
  once (guards against double-injection and against silently using the
  occupied `BeforeModelPolicy` slot).
- `pending_skill_buffer_is_cleared_after_injection` — a skill activates
  for the **next** turn only, not every subsequent turn.
- `slash_command_info_equality_unchanged_after_cow_migration` — the
  PR 1 refactor is behavior-preserving.

No new persisted type or field is introduced (the injected message is a
plain `UserMessage`), so **no `CURRENT_SESSION_SCHEMA_VERSION` bump and
no forward-compat fixture** are required. This is asserted by
`staged_skill_body_is_prepended_to_next_prompt_turn` reading back an
ordinary `Message::User`.

---

## 6. Risks

| Risk | Mitigation / punt |
|---|---|
| `Cow` migration ripples to many `info.name` call sites and silently breaks comparisons. | Isolated as PR 1 with no behavior change; `grep` for every `.name`/`.summary`/`.argument_hint` use (per `CLAUDE.md` "No Semantic Search"); existing coverage tests must stay green. |
| Hand-rolled frontmatter parser diverges from real YAML on quoting/multiline edge cases. | Restrict to a documented subset; reject (typed error) anything outside it rather than mis-parse. Escalation path: named `serde_yml = "0.0.12"` dep if spec-completeness is required. |
| Injecting via a synthetic `Message::User` competes with `ContextVirtualizationPolicy` archiving/eviction. | The injected message is an ordinary turn the existing machinery already handles; no new policy, no second `BeforeModelPolicy`. Verified single-slot constraint (`agent_loop.rs:415`). |
| A skill body could be large and blow the context budget. | Out of scope for the thin loader; the existing compaction/virtualization machinery applies to it like any user turn. Note as a known limitation, do not add per-skill size caps speculatively. |
| `allowed-tools` is parsed but not enforced — user expects tool restriction. | Parse + store + (optionally) name the expected tools in the injected block so the model sees them; hard enforcement deferred (§8) and called out in the activation confirmation message. |
| Startup latency from scanning skill dirs on every launch. | Discovery is a shallow `read_dir` of two directories; bounded and one-time at bootstrap. If it ever shows up in a smoke trace, cache by mtime — not before. |

---

## 7. Exit criteria

- [ ] `SlashCommandSource::Skill` is **constructed** at runtime (closes
      EXT-2's "never instantiated").
- [ ] `CommandRegistry` registers skill commands at runtime (closes
      EXT-4's "no mechanism to append").
- [ ] `SKILL.md` files under `~/.anie/skills/` and project
      `.anie/skills/` are discovered, with project precedence.
- [ ] Frontmatter `name`, `description`, `allowed-tools` parsed; malformed
      files warn and are skipped, never panic.
- [ ] `/skill:name` appears in `/help` under a `Skills:` group and in
      autocomplete.
- [ ] Invoking `/skill:name` injects the skill body as the next turn's
      context exactly once; buffer clears.
- [ ] `cargo test --workspace` green; `cargo clippy --workspace
      --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
- [ ] `cargo tree -p anie-cli` shows no new YAML dependency (or, if the
      `serde_yml` fallback is taken, the dep is justified in the PR body).
- [ ] No `CURRENT_SESSION_SCHEMA_VERSION` bump needed (no persisted-type
      field added) — asserted by test.
- [ ] Manual smoke per `docs/smoke_protocol_2026-05-01.md` passes.
- [ ] `docs/arch/anie-rs_architecture.md` updated (skills loader +
      `/skill:name` dispatch path).
- [ ] `docs/ROADMAP.md` item 10 ("Skills system") marked landed.

---

## 8. Deferred

Explicitly **not** done here (considered, scoped out):

- **The full Plan-10 extension system.** Out-of-process JSON-RPC
  subprocess transport, `ExtensionRunner`/`ExtensionHost`, manifest
  discovery, dynamic provider/renderer registration
  (`docs/refactor_plans/10_extension_system_pi_port.md`, EXT-1/EXT-5/
  EXT-6). The rival analysis deprioritizes it in favor of an MCP client
  (`docs/rival_analysis_2026-06-06/README.md`, initiative #2). This
  loader needs none of it.
- **Hard `allowed-tools` enforcement.** Parsing + storing the field is
  in scope; restricting the `ToolRegistry` per active skill at tool-call
  time is not. Our injection model (stage body for the next turn) has no
  persistent "skill is active" mode to scope a tool set to. When added,
  rejection would route through `ToolError`, not `ProviderError`.
- **Auto-injection / `disable-model-invocation`.** pi can auto-load
  skill descriptions into the system prompt so the model self-selects.
  We require explicit `/skill:name` invocation. Auto-listing skill
  descriptions in the system prompt is a clean follow-up but not needed
  to close EXT-2/EXT-4.
- **`/skills` listing command and `/reload` re-scan.** Nice-to-haves
  from `docs/notes/skills_system.md`; `/help` already surfaces the
  Skills group, and `/reload` integration can follow once the loader is
  proven.
- **A real YAML parser by default.** Hand-rolled subset ships unless
  review demands spec-completeness; `serde_yml` is the named fallback.
- **Skill packaging / remote skills.** pi loads skills from packages;
  we load from two local directories only.
- **Persisting active-skill state across sessions.** Skills are
  re-discovered each launch; activation is per-turn and not persisted,
  so no session schema change.
