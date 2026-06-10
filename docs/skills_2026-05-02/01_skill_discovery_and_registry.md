# PR 1 — Skill discovery + registry

## Rationale

Skills only work if the agent knows they exist. PR 1
builds the discovery + registry layer: read skill
markdown files from disk, parse frontmatter, expose a
catalog (name + description) that the system prompt
can surface and the `skill` tool (PR 2) can index.

No tool yet — this PR is plumbing.

## Design

### File locations

After reading codex's
(`/home/daniel/Projects/agents/codex/codex-rs/core-skills/src/loader.rs`)
and pi's
(`/home/daniel/Projects/agents/pi/packages/coding-agent/src/core/skills.ts`)
implementations, we'll match the emerging convention:
**harness-specific path AND a shared `.agents/skills/`
path**. Both repos do this — pi looks at `~/.pi/skills/`
AND `~/.agents/skills/`; codex looks at
`~/.agents/skills/` AND `$CODEX_HOME/skills/`. The
shared `.agents/skills/` lets a user write a skill
once and have multiple harnesses load it.

Four layers, merged with
**project-anie > project-shared > user-anie > user-shared > bundled**
precedence on name collisions:

1. **Bundled** — `crates/anie-cli/skills/`. Ships with
   the binary (loaded from disk in debug, embedded
   via `include_str!` for release).
2. **User-shared** — `~/.agents/skills/`. Cross-
   harness skills.
3. **User-anie** — `~/.anie/skills/`. anie-specific
   user skills.
4. **Project-shared** — `<cwd>/.agents/skills/`.
   Cross-harness, checked into the project.
5. **Project-anie** — `<cwd>/.anie/skills/`. anie-
   specific, checked into the project.

### File format — both single `.md` and directory-with-`SKILL.md`

The Agent Skills standard (which both pi and codex
follow) supports two layouts:

1. **Single file:** `<path>/<skill_name>.md` —
   simple skills with no supporting files. The body
   is the markdown content after the frontmatter.
2. **Directory:** `<path>/<skill_name>/SKILL.md` —
   the directory can also contain `references/`,
   `scripts/`, `assets/`. The agent resolves relative
   paths inside the body against the skill's
   directory.

Codex supports only the directory layout; pi supports
both depending on which root the skill lives in. We
support both: in any of the five layers, both root
`.md` files (skill name = filename stem) and
subdirectory `<name>/SKILL.md` are valid. Bundled
skills will start with the simple `.md` form; users
can opt into the directory form when a skill needs
scripts or reference files.

The directory form is what unlocks the "skill includes
a Python validator script the agent can run" pattern.
We don't ship that today, but the format choice
preserves the option.

### Frontmatter schema — match the Agent Skills standard

Both reference repos require `name` and `description`.
We'll match that. Optional fields adopt only what
multiple repos use to stay close to the standard:

```yaml
---
name: cpp_rule_of_five              # required
description: Brief catalog blurb.   # required
disable_model_invocation: false     # optional, pi convention; default false
license: MIT                        # optional, ignored at runtime today
---
```

We deliberately defer the heavier fields (codex's
`interface.*`, `dependencies.tools[]`,
`policy.products[]`) until a real use case appears.
They're easy to add as additive fields later.

`name` validation matches the standard: 1–64 chars,
lowercase a-z 0-9 hyphens, no leading/trailing/
consecutive hyphens. Same regex as both reference
repos.

The `when_to_use` field I sketched in the README isn't
in either reference implementation — both treat
`description` as carrying that information. Drop it
to stay standards-compliant; if descriptions get
unwieldy we can add structured fields later.

### Skill type

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub disable_model_invocation: bool,
    pub body: String,
    pub source: SkillSource,           // Bundled | UserShared | UserAnie | ProjectShared | ProjectAnie
    pub root_dir: PathBuf,             // skill's directory (for relative-path resolution in body)
    pub manifest_path: PathBuf,        // the .md or SKILL.md file itself
}
```

`root_dir` is the directory containing the manifest.
For single-file skills, that's the parent of the
`.md`; for directory-form, it's the skill's own
directory. The body's relative references (e.g.,
`scripts/validate.py`) are resolved against this.

Body is loaded eagerly on registry build (skills are
small; no need for lazy loading). If a skill body
exceeds a threshold (default 4096 tokens, env var
`ANIE_SKILL_MAX_BODY_TOKENS`), log a warning at
registry-build time but accept it — the user knows
what they wrote.

### Registry

```rust
pub struct SkillRegistry {
    by_name: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn discover(cwd: &Path) -> Result<Self>;
    pub fn get(&self, name: &str) -> Option<&Skill>;
    pub fn catalog(&self) -> Vec<SkillCatalogEntry>;
}

pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
}
```

`discover` walks bundled → user → project, parses each
`.md` file, applies precedence, returns the merged
registry. Failures on individual skills (malformed
frontmatter, missing required fields) log a warning
and skip the skill — never abort discovery.

### System-prompt surfacing

Add a `{skill_catalog}` placeholder to the prompt
template. When skills are present, render:

```
Available skills (load with the `skill` tool when
relevant):
- cpp_rule_of_five: When implementing a C++ class
  that owns raw `new`/`delete` allocations…
- use_recurse_for_archive_lookup: When the user asks
  a follow-up that…
…
```

When no skills are loaded, omit the section entirely
(don't bloat the prompt with empty placeholders).

The catalog goes into the BASE prompt (not the rlm
augment) because skills work in all harness modes.

## Files to touch

- New: `crates/anie-cli/src/skills.rs` — `Skill`,
  `SkillRegistry`, `discover`, `parse_skill_file`.
- New: `crates/anie-cli/skills/` — directory for
  bundled skills (initially empty; PR 3 populates).
- `crates/anie-cli/src/controller.rs` —
  `build_system_prompt` reads the skill catalog and
  renders into the prompt.
- `crates/anie-cli/src/lib.rs` — export `Skill`,
  `SkillRegistry`.
- `crates/anie-cli/Cargo.toml` — add `serde_yaml` for
  frontmatter parsing (or `serde_yml` if already in
  the tree).
- Tests in the same crate.

Estimated diff: ~300 LOC of code, ~150 LOC of tests.

## Phased PRs

Single PR.

## Test plan

- `parse_skill_file_extracts_frontmatter_and_body`
  — happy path with a sample markdown file.
- `parse_skill_file_rejects_missing_required_field`
  — frontmatter without `name` or `description` is
  skipped with a warning.
- `parse_skill_file_handles_no_frontmatter`
  — a `.md` file without frontmatter is skipped.
- `registry_discover_merges_bundled_user_project_with_precedence`
  — same skill name in two layers; project wins.
- `registry_discover_logs_warning_for_oversize_body`
  — synthesize a 5000-token body; assert warning logged.
- `system_prompt_renders_skill_catalog_when_skills_present`
  — build a prompt with skills present, assert the
  catalog block appears.
- `system_prompt_omits_skill_catalog_when_empty`
  — no skills → no `Available skills:` block in the
  prompt.

## Risks

- **System prompt bloat.** Each skill adds ~60-100
  tokens to the catalog. With 20 skills that's
  significant. Mitigation: catalog is always-on but
  bodies are lazy. The 4096-token body cap keeps
  individual loads bounded.
- **Frontmatter parsing fragility.** YAML is
  notoriously easy to mis-format. Mitigation:
  forgiving parser, log+skip rather than fail.
- **Cache invalidation.** System prompt is cached
  (see `runtime/prompt_cache.rs`). Adding skills to
  the prompt means catalog changes invalidate the
  cache. Mitigation: skills are read at startup; the
  catalog doesn't change mid-session. Cache
  invalidates only across runs, which is fine.

## Exit criteria

- [ ] `Skill`, `SkillRegistry` exist and are tested.
- [ ] `discover()` reads bundled/user/project, applies
      precedence.
- [ ] Malformed/oversize skills are logged + skipped
      (never abort).
- [ ] System prompt renders the catalog when skills
      are present, omits the section when empty.
- [ ] `cargo test --workspace` + clippy clean.

## Deferred

- Skill aliases / shortcuts (e.g., `--skill cpp` for
  `cpp_rule_of_five`).
- Hot reload of skills on file change. Today's design
  reads at session start; iterating on a skill
  requires a restart. Add only if the iteration loop
  becomes painful.
- Skill versioning / dependencies. Treat each skill as
  standalone for now.
- Subdirectory scan for project/user skills.
