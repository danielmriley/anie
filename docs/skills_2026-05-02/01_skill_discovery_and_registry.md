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

Three layers, merged with **project > user > bundled**
precedence on name collisions:

1. **Bundled** — shipped inside the anie binary at
   `crates/anie-cli/skills/*.md` (loaded from disk in
   debug builds, embedded via `include_str!` for
   release builds).
2. **User** — `~/.anie/skills/*.md`. Created by the
   user; persists across sessions.
3. **Project** — `<cwd>/.anie/skills/*.md`. Per-repo
   guidance; checked into the project.

Recursive subdirectory scan is deferred (project
skills can use namespacing in filenames if needed —
e.g., `cpp_rule_of_five.md`).

### Skill type

```rust
pub struct Skill {
    pub name: String,            // unique identifier
    pub description: String,     // one-liner for catalog
    pub when_to_use: String,     // hint for the agent
    pub body: String,            // markdown body (lazy?)
    pub source: SkillSource,     // Bundled | User | Project
    pub path: PathBuf,           // for diagnostics
}
```

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
