//! Skill discovery + registry. PR 1 of
//! `docs/skills_2026-05-02/`.
//!
//! A skill is a markdown file with YAML frontmatter that the
//! agent can load on-demand to bring focused guidance into
//! its working context. PR 1 builds the discovery + registry
//! layer; PR 2 adds the `skill` tool that loads bodies; PR 3
//! ships the initial bundled set.
//!
//! ## File layout
//!
//! Two layouts are accepted, matching the Agent Skills
//! standard ([agentskills.io/specification](https://agentskills.io/specification)):
//!
//! 1. **Single file:** `<root>/<skill_name>.md` — simple
//!    skills with no supporting files.
//! 2. **Directory:** `<root>/<skill_name>/SKILL.md` — the
//!    directory can also contain `references/`, `scripts/`,
//!    `assets/`. Relative paths inside the body resolve
//!    against the skill's directory.
//!
//! ## Discovery roots
//!
//! Five layers, in precedence order (highest first):
//!
//! 1. Project-anie (`<cwd>/.anie/skills/`)
//! 2. Project-shared (`<cwd>/.agents/skills/`)
//! 3. User-anie (`~/.anie/skills/`)
//! 4. User-shared (`~/.agents/skills/`)
//! 5. Bundled (`crates/anie-cli/skills/`)
//!
//! Layer precedence reflects the convention shared by codex
//! (`codex-rs/core-skills/src/loader.rs`) and pi
//! (`packages/coding-agent/src/core/skills.ts`): more-local
//! wins over more-shared. Skill names that collide across
//! layers resolve to the higher-precedence layer; lower-
//! precedence definitions are dropped silently (PR 4 will
//! add a `/skills` slash command that surfaces shadowed
//! skills for diagnostics).

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tracing::warn;

/// Default skill root inside the user $HOME, harness-specific.
pub const USER_ANIE_SKILLS_DIR: &str = ".anie/skills";
/// Cross-harness skill root inside the user $HOME (codex/pi
/// convention).
pub const USER_SHARED_SKILLS_DIR: &str = ".agents/skills";
/// Anthropic-standard skill root inside the user $HOME. Lets
/// users with existing Claude Code skills get them loaded by
/// anie automatically without re-creating files.
pub const USER_CLAUDE_SKILLS_DIR: &str = ".claude/skills";
/// Default skill root inside a project, harness-specific.
pub const PROJECT_ANIE_SKILLS_DIR: &str = ".anie/skills";
/// Cross-harness skill root inside a project.
pub const PROJECT_SHARED_SKILLS_DIR: &str = ".agents/skills";
/// Anthropic-standard skill root inside a project. Picks up
/// existing Claude Code project skills.
pub const PROJECT_CLAUDE_SKILLS_DIR: &str = ".claude/skills";

/// Soft cap on a skill body in characters. Skills that exceed
/// this still load but emit a `warn!` log so users notice the
/// large skill is competing with active context. The byte cap
/// is a coarse proxy for token count (ratio is ~3-4 bytes/token
/// for English; this corresponds roughly to ~4096 tokens).
pub const SKILL_BODY_BYTE_WARN_THRESHOLD: usize = 16_384;

/// Where a skill was discovered. Higher-precedence layers
/// shadow lower ones on name collisions. Order in this enum
/// matches the precedence order, highest first.
///
/// Within project / user tiers, the harness-specific path
/// (`.anie/skills/`) wins over the codex/pi convention
/// (`.agents/skills/`), which wins over Anthropic's
/// `.claude/skills/`. The reasoning: when a user puts a skill
/// under `.anie/`, they explicitly want it for this harness;
/// `.agents/` is the cross-harness "I want this everywhere"
/// path; `.claude/` is the imported-from-elsewhere fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    ProjectAnie,
    ProjectShared,
    ProjectClaude,
    UserAnie,
    UserShared,
    UserClaude,
    Bundled,
}

impl SkillSource {
    fn precedence(self) -> u8 {
        match self {
            Self::ProjectAnie => 0,
            Self::ProjectShared => 1,
            Self::ProjectClaude => 2,
            Self::UserAnie => 3,
            Self::UserShared => 4,
            Self::UserClaude => 5,
            Self::Bundled => 6,
        }
    }

    /// Short label for logs and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::ProjectAnie => "project-anie",
            Self::ProjectShared => "project-shared",
            Self::ProjectClaude => "project-claude",
            Self::UserAnie => "user-anie",
            Self::UserShared => "user-shared",
            Self::UserClaude => "user-claude",
            Self::Bundled => "bundled",
        }
    }
}

/// Frontmatter we parse out of a skill manifest. Only `name`
/// and `description` are required. Extra fields (e.g. codex's
/// `interface.*`, `dependencies.tools[]`) are ignored —
/// adding them later is additive.
#[derive(Debug, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    disable_model_invocation: bool,
    #[serde(default)]
    license: Option<String>,
}

/// A loaded skill. Body is held eagerly because skills are
/// expected to be small (<16KB cap, ~4k tokens); lazy loading
/// would complicate the type for negligible memory savings.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub disable_model_invocation: bool,
    pub license: Option<String>,
    pub body: String,
    pub source: SkillSource,
    /// Directory containing the manifest. For single-file
    /// skills this is the parent of the `.md`; for directory-
    /// form skills it's the skill's own directory. Body
    /// references like `scripts/validate.py` resolve against
    /// this.
    pub root_dir: PathBuf,
    /// Path to the `.md` or `SKILL.md` file itself.
    pub manifest_path: PathBuf,
}

/// Catalog entry — what the system prompt surfaces to the
/// model. Keep it minimal so the catalog stays cheap on cache.
#[derive(Debug, Clone)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
}

/// Registry of discovered skills, indexed by name.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    by_name: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// Empty registry — used by tests and when discovery is
    /// disabled.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Walk the five discovery layers in **lowest-precedence
    /// first** order so higher layers can overwrite. Failures
    /// on individual skill files are logged and skipped, never
    /// aborting discovery — a single malformed manifest in
    /// `~/.agents/skills/` shouldn't break the harness.
    pub fn discover(cwd: &Path) -> Self {
        let mut registry = Self::empty();
        // Bundled skills are embedded via `include_str!` so they
        // load without needing CARGO_MANIFEST_DIR or a real
        // filesystem path — works in `cargo run` AND release
        // binaries.
        for skill in bundled_skills() {
            registry.insert_with_precedence(skill);
        }
        for (root, source) in discovery_roots(cwd) {
            registry.absorb_root(&root, source);
        }
        registry
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.by_name.get(name)
    }

    /// Iterate over all skills, sorted by name. Sorting keeps
    /// the system-prompt catalog stable — important for
    /// prompt-cache hits across runs.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        let mut skills: Vec<&Skill> = self.by_name.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills.into_iter()
    }

    /// True when no skills are registered.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Number of registered skills.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Catalog of (name, description) entries the system
    /// prompt surfaces. Skills marked `disable_model_invocation`
    /// are excluded — they're loadable via slash commands but
    /// shouldn't appear in the agent-facing catalog.
    pub fn catalog(&self) -> Vec<SkillCatalogEntry> {
        self.iter()
            .filter(|skill| !skill.disable_model_invocation)
            .map(|skill| SkillCatalogEntry {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect()
    }

    /// Insert a skill, replacing any prior entry with the
    /// same name only if the new source has higher (or equal)
    /// precedence. Equal precedence handling: keep the
    /// existing entry (first-loaded wins within a single
    /// layer). Loading order within a directory is sorted to
    /// keep this deterministic.
    fn insert_with_precedence(&mut self, skill: Skill) {
        match self.by_name.get(&skill.name) {
            Some(existing) if existing.source.precedence() <= skill.source.precedence() => {
                // Existing entry has equal-or-higher precedence
                // (lower number = higher precedence). Keep it.
                tracing::debug!(
                    name = %skill.name,
                    existing_source = existing.source.label(),
                    shadowed_source = skill.source.label(),
                    shadowed_path = %skill.manifest_path.display(),
                    "skill shadowed by higher-precedence entry"
                );
            }
            _ => {
                self.by_name.insert(skill.name.clone(), skill);
            }
        }
    }

    /// Test-only entry point for inserting skills from a
    /// specific root with a specific source. Production code
    /// should use [`Self::discover`].
    #[cfg(test)]
    pub(crate) fn absorb_root_for_test(&mut self, root: &Path, source: SkillSource) {
        self.absorb_root(root, source);
    }

    fn absorb_root(&mut self, root: &Path, source: SkillSource) {
        if !root.is_dir() {
            return;
        }
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) => {
                warn!(
                    root = %root.display(),
                    %error,
                    "failed to read skill discovery root"
                );
                return;
            }
        };
        // Sort entries for deterministic load order within a
        // single layer (stable behavior across filesystem
        // traversal orderings).
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        paths.sort();
        for path in paths {
            match parse_entry(&path, source) {
                Ok(Some(skill)) => self.insert_with_precedence(skill),
                Ok(None) => {} // not a skill file/dir, silently skip
                Err(error) => {
                    warn!(
                        path = %path.display(),
                        %error,
                        "failed to load skill — skipping"
                    );
                }
            }
        }
    }
}

/// Resolve the six filesystem discovery roots in
/// **lowest-precedence first** order (so the registry's load
/// loop can let higher layers overwrite). Layers whose paths
/// can't be resolved (e.g. no `$HOME`) are silently omitted.
/// Bundled skills are NOT included here — they're embedded
/// via [`include_str!`] in [`BUNDLED_SKILL_MANIFESTS`] and
/// loaded into the registry separately.
fn discovery_roots(cwd: &Path) -> Vec<(PathBuf, SkillSource)> {
    let mut roots = Vec::with_capacity(6);
    if let Some(home) = dirs::home_dir() {
        roots.push((home.join(USER_CLAUDE_SKILLS_DIR), SkillSource::UserClaude));
        roots.push((home.join(USER_SHARED_SKILLS_DIR), SkillSource::UserShared));
        roots.push((home.join(USER_ANIE_SKILLS_DIR), SkillSource::UserAnie));
    }
    roots.push((
        cwd.join(PROJECT_CLAUDE_SKILLS_DIR),
        SkillSource::ProjectClaude,
    ));
    roots.push((
        cwd.join(PROJECT_SHARED_SKILLS_DIR),
        SkillSource::ProjectShared,
    ));
    roots.push((cwd.join(PROJECT_ANIE_SKILLS_DIR), SkillSource::ProjectAnie));
    roots
}

/// The four bundled skills, embedded into the binary at
/// compile time via `include_str!`. Loaded into the registry
/// at startup before the filesystem layers so any user/project
/// skill of the same name shadows the bundled version.
///
/// Adding a new bundled skill: drop the `.md` file in
/// `crates/anie-cli/skills/` and add an entry to this array.
/// The filename stem and frontmatter `name` MUST match (the
/// PR 1 parser enforces this).
const BUNDLED_SKILL_MANIFESTS: &[(&str, &str)] = &[
    (
        "cpp-rule-of-five",
        include_str!("../skills/cpp-rule-of-five.md"),
    ),
    (
        "decompose-multi-constraint-task",
        include_str!("../skills/decompose-multi-constraint-task.md"),
    ),
    (
        "use-recurse-for-archive-lookup",
        include_str!("../skills/use-recurse-for-archive-lookup.md"),
    ),
    (
        "verify-after-edit",
        include_str!("../skills/verify-after-edit.md"),
    ),
];

/// Parse the embedded bundled skill manifests into `Skill`
/// values. Failures here are programming errors (a malformed
/// bundled manifest would never have shipped) so we
/// `expect` rather than log-and-skip.
fn bundled_skills() -> Vec<Skill> {
    BUNDLED_SKILL_MANIFESTS
        .iter()
        .map(|(name, raw)| {
            parse_manifest_from_text(raw, name, SkillSource::Bundled).unwrap_or_else(|err| {
                panic!("bundled skill `{name}` failed to parse: {err}")
            })
        })
        .collect()
}

/// Parse a discovery-root entry into a skill, if it represents
/// one. Returns `Ok(None)` for entries that aren't skill files
/// (e.g. a `.gitkeep`); returns `Err` only on real load errors
/// (parse failure, missing required field, IO error).
fn parse_entry(path: &Path, source: SkillSource) -> Result<Option<Skill>> {
    if path.is_file() {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("md"))
        {
            return Ok(None);
        }
        // Single-file skill: name = filename stem.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("skill manifest has no filename stem"))?;
        let body_root = path.parent().unwrap_or(path).to_path_buf();
        return Ok(Some(parse_manifest(path, &body_root, Some(stem), source)?));
    }
    if path.is_dir() {
        let manifest = path.join("SKILL.md");
        if !manifest.is_file() {
            return Ok(None);
        }
        // Directory-form skill: name comes from frontmatter,
        // but the directory name is the canonical anchor for
        // body relative paths.
        return Ok(Some(parse_manifest(&manifest, path, None, source)?));
    }
    Ok(None)
}

/// Parse a single on-disk manifest. `expected_name` is
/// `Some(stem)` for single-file skills (we cross-check that
/// frontmatter `name` matches the filename stem) and `None`
/// for directory-form skills (the directory is the source of
/// truth).
fn parse_manifest(
    manifest_path: &Path,
    body_root: &Path,
    expected_name: Option<&str>,
    source: SkillSource,
) -> Result<Skill> {
    let raw = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let mut skill = parse_manifest_from_text(&raw, expected_name.unwrap_or(""), source)
        .with_context(|| format!("parsing {}", manifest_path.display()))?;
    if let Some(expected) = expected_name {
        if skill.name != expected {
            return Err(anyhow!(
                "frontmatter name `{}` does not match filename stem `{}`",
                skill.name,
                expected
            ));
        }
    }
    skill.root_dir = body_root.to_path_buf();
    skill.manifest_path = manifest_path.to_path_buf();
    Ok(skill)
}

/// Shared frontmatter+body parsing for both on-disk and
/// embedded manifests. The embedded path passes the skill
/// name as the `expected_name` (or empty string to skip the
/// cross-check) and ignores the resulting `root_dir` /
/// `manifest_path` since neither is meaningful for embedded
/// skills.
fn parse_manifest_from_text(
    raw: &str,
    expected_name: &str,
    source: SkillSource,
) -> Result<Skill> {
    let (frontmatter_text, body) =
        split_frontmatter(raw).context("malformed frontmatter")?;
    let fm: SkillFrontmatter = serde_yml::from_str(frontmatter_text)
        .context("parsing YAML frontmatter")?;
    validate_skill_name(&fm.name).context("invalid skill name")?;
    if !expected_name.is_empty() && fm.name != expected_name {
        return Err(anyhow!(
            "frontmatter name `{}` does not match expected `{}`",
            fm.name,
            expected_name
        ));
    }
    if fm.description.trim().is_empty() {
        return Err(anyhow!("skill description must be non-empty"));
    }
    if body.len() > SKILL_BODY_BYTE_WARN_THRESHOLD {
        warn!(
            name = %fm.name,
            body_bytes = body.len(),
            threshold = SKILL_BODY_BYTE_WARN_THRESHOLD,
            "skill body exceeds soft cap; may pressure active context"
        );
    }
    Ok(Skill {
        name: fm.name,
        description: fm.description,
        disable_model_invocation: fm.disable_model_invocation,
        license: fm.license,
        body: body.to_string(),
        source,
        root_dir: PathBuf::new(),
        manifest_path: PathBuf::new(),
    })
}

/// Strict skill-name validation matching both reference repos:
/// 1-64 chars, lowercase a-z, 0-9, hyphens. No leading or
/// trailing hyphen, no consecutive hyphens.
fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(anyhow!(
            "name must be 1-64 chars (got {})",
            name.len()
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(anyhow!("name must not start or end with `-`"));
    }
    let mut last_was_hyphen = false;
    for ch in name.chars() {
        let allowed = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-';
        if !allowed {
            return Err(anyhow!(
                "name must contain only lowercase a-z, 0-9, and `-` (found `{ch}`)"
            ));
        }
        if ch == '-' && last_was_hyphen {
            return Err(anyhow!("name must not contain consecutive hyphens"));
        }
        last_was_hyphen = ch == '-';
    }
    Ok(())
}

/// Split a markdown source into `(frontmatter_yaml, body)` if a
/// `---` frontmatter block is present at the start. Returns
/// `Err` if the file claims a frontmatter block (`---` on the
/// first line) but doesn't close it.
fn split_frontmatter(raw: &str) -> Result<(&str, &str)> {
    let trimmed = raw.trim_start_matches('\u{feff}'); // strip BOM if present
    if !trimmed.starts_with("---") {
        return Err(anyhow!("missing frontmatter — expected `---` on first line"));
    }
    let after_open = trimmed
        .strip_prefix("---")
        .and_then(|s| s.strip_prefix('\n').or_else(|| s.strip_prefix("\r\n")))
        .ok_or_else(|| anyhow!("frontmatter open `---` must be followed by a newline"))?;
    // Find the closing `---` on its own line.
    let close_marker_pos = after_open
        .find("\n---")
        .ok_or_else(|| anyhow!("frontmatter not closed — expected `---` on its own line"))?;
    let frontmatter_text = &after_open[..close_marker_pos];
    let after_close = &after_open[close_marker_pos + "\n---".len()..];
    let body = after_close
        .strip_prefix('\n')
        .or_else(|| after_close.strip_prefix("\r\n"))
        .unwrap_or(after_close);
    Ok((frontmatter_text, body))
}

/// Render the skill catalog as a system-prompt fragment.
/// Returns an empty string when the registry is empty so
/// callers can append unconditionally without ending up with
/// stray "Available skills:" headers.
pub fn render_catalog(registry: &SkillRegistry) -> String {
    let entries = registry.catalog();
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "Available skills (load with the `skill` tool when relevant):\n",
    );
    for entry in entries {
        // Single-line description: replace any embedded
        // newlines with spaces so the catalog stays one
        // entry per line. Long descriptions wrap visually
        // in the model's tokenization, which is fine.
        let one_line = entry.description.replace('\n', " ");
        out.push_str(&format!("- {}: {}\n", entry.name, one_line));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_skill(dir: &Path, filename: &str, body: &str) {
        fs::create_dir_all(dir).expect("create dir");
        fs::write(dir.join(filename), body).expect("write skill");
    }

    fn sample_skill(name: &str, description: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {description}\n---\n\n# Body\n\nGuidance text.\n"
        )
    }

    #[test]
    fn parse_skill_file_extracts_frontmatter_and_body() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("simple.md");
        fs::write(&path, sample_skill("simple", "A simple skill")).expect("write");
        let skill = parse_entry(&path, SkillSource::Bundled)
            .expect("ok")
            .expect("some");
        assert_eq!(skill.name, "simple");
        assert_eq!(skill.description, "A simple skill");
        assert!(skill.body.contains("Guidance text"));
        assert!(!skill.disable_model_invocation);
    }

    #[test]
    fn parse_skill_file_rejects_missing_required_fields() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("broken.md");
        fs::write(&path, "---\ndescription: only description\n---\nbody").expect("write");
        let result = parse_entry(&path, SkillSource::Bundled);
        assert!(result.is_err(), "missing name should be rejected");
    }

    #[test]
    fn parse_skill_file_rejects_filename_name_mismatch() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("not-matching.md");
        fs::write(&path, sample_skill("different-name", "x")).expect("write");
        let result = parse_entry(&path, SkillSource::Bundled);
        assert!(
            result.is_err(),
            "single-file skill with mismatched filename stem should fail"
        );
    }

    #[test]
    fn parse_skill_file_rejects_invalid_name() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("Bad_Name.md");
        fs::write(&path, sample_skill("Bad_Name", "x")).expect("write");
        let result = parse_entry(&path, SkillSource::Bundled);
        assert!(result.is_err(), "uppercase + underscore name should fail");
    }

    #[test]
    fn parse_skill_file_handles_no_frontmatter() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("plain.md");
        fs::write(&path, "# Just a markdown file\n").expect("write");
        let result = parse_entry(&path, SkillSource::Bundled);
        assert!(result.is_err(), "missing frontmatter should fail loudly");
    }

    #[test]
    fn parse_skill_directory_form_loads_skill_md() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("complex-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            sample_skill("complex-skill", "Complex"),
        )
        .expect("write SKILL.md");
        let skill = parse_entry(&skill_dir, SkillSource::Bundled)
            .expect("ok")
            .expect("some");
        assert_eq!(skill.name, "complex-skill");
        assert_eq!(skill.root_dir, skill_dir);
    }

    #[test]
    fn parse_skill_directory_form_returns_none_without_skill_md() {
        let dir = tempdir().expect("tempdir");
        let skill_dir = dir.path().join("not-a-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(skill_dir.join("README.md"), "not a skill").expect("write");
        let result = parse_entry(&skill_dir, SkillSource::Bundled).expect("ok");
        assert!(result.is_none(), "directory without SKILL.md should be skipped");
    }

    #[test]
    fn registry_discover_merges_with_precedence() {
        // Project-anie should win over user-anie.
        let cwd_dir = tempdir().expect("cwd tempdir");
        let home_dir = tempdir().expect("home tempdir");

        let project_root = cwd_dir.path().join(PROJECT_ANIE_SKILLS_DIR);
        let user_root = home_dir.path().join(USER_ANIE_SKILLS_DIR);
        write_skill(
            &project_root,
            "shared-name.md",
            &sample_skill("shared-name", "project version"),
        );
        write_skill(
            &user_root,
            "shared-name.md",
            &sample_skill("shared-name", "user version"),
        );

        // Build a registry that mimics the discovery flow with
        // overridden roots. We do this by exercising the
        // public absorb_root via a fresh registry, since
        // `discover` reads $HOME directly and we don't want to
        // mutate that for tests.
        let mut registry = SkillRegistry::empty();
        // Lowest-precedence first:
        registry.absorb_root(&user_root, SkillSource::UserAnie);
        registry.absorb_root(&project_root, SkillSource::ProjectAnie);

        let skill = registry.get("shared-name").expect("present");
        assert_eq!(skill.description, "project version");
        assert_eq!(skill.source, SkillSource::ProjectAnie);
    }

    #[test]
    fn registry_excludes_disable_model_invocation_from_catalog() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("skills");
        write_skill(
            &root,
            "visible.md",
            &sample_skill("visible", "Shows in catalog"),
        );
        write_skill(
            &root,
            "hidden.md",
            "---\nname: hidden\ndescription: Hidden\ndisable_model_invocation: true\n---\nbody\n",
        );
        let mut registry = SkillRegistry::empty();
        registry.absorb_root(&root, SkillSource::Bundled);
        let catalog: Vec<String> = registry.catalog().into_iter().map(|e| e.name).collect();
        assert_eq!(catalog, vec!["visible".to_string()]);
        // But `get` still returns the hidden skill — it's
        // loadable via slash command (PR 4).
        assert!(registry.get("hidden").is_some());
    }

    #[test]
    fn registry_iter_returns_skills_sorted_by_name() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("skills");
        write_skill(&root, "z-skill.md", &sample_skill("z-skill", "z"));
        write_skill(&root, "a-skill.md", &sample_skill("a-skill", "a"));
        write_skill(&root, "m-skill.md", &sample_skill("m-skill", "m"));
        let mut registry = SkillRegistry::empty();
        registry.absorb_root(&root, SkillSource::Bundled);
        let names: Vec<String> = registry.iter().map(|s| s.name.clone()).collect();
        assert_eq!(
            names,
            vec!["a-skill".to_string(), "m-skill".to_string(), "z-skill".to_string()]
        );
    }

    #[test]
    fn render_catalog_returns_empty_string_when_no_skills() {
        let registry = SkillRegistry::empty();
        assert_eq!(render_catalog(&registry), "");
    }

    #[test]
    fn render_catalog_lists_skills_one_per_line() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("skills");
        write_skill(
            &root,
            "alpha.md",
            &sample_skill("alpha", "First skill"),
        );
        write_skill(
            &root,
            "beta.md",
            &sample_skill("beta", "Second skill"),
        );
        let mut registry = SkillRegistry::empty();
        registry.absorb_root(&root, SkillSource::Bundled);
        let rendered = render_catalog(&registry);
        assert!(rendered.starts_with("Available skills"));
        assert!(rendered.contains("- alpha: First skill"));
        assert!(rendered.contains("- beta: Second skill"));
    }

    #[test]
    fn render_catalog_collapses_multiline_descriptions_to_one_line() {
        // YAML folded scalars or embedded newlines could
        // result in multi-line descriptions; render them on
        // a single catalog line.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("multi.md");
        fs::write(
            &path,
            "---\nname: multi\ndescription: |\n  First line\n  second line\n---\nbody\n",
        )
        .expect("write");
        let mut registry = SkillRegistry::empty();
        registry.absorb_root(dir.path(), SkillSource::Bundled);
        let rendered = render_catalog(&registry);
        assert!(rendered.contains("- multi: First line second line"), "got:\n{rendered}");
    }

    #[test]
    fn validate_skill_name_accepts_standard_form() {
        assert!(validate_skill_name("cpp-rule-of-five").is_ok());
        assert!(validate_skill_name("a").is_ok());
        assert!(validate_skill_name("foo123").is_ok());
    }

    #[test]
    fn validate_skill_name_rejects_violations() {
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("-leading").is_err());
        assert!(validate_skill_name("trailing-").is_err());
        assert!(validate_skill_name("double--hyphen").is_err());
        assert!(validate_skill_name("UPPERCASE").is_err());
        assert!(validate_skill_name("under_score").is_err());
        assert!(validate_skill_name("space here").is_err());
        let too_long = "a".repeat(65);
        assert!(validate_skill_name(&too_long).is_err());
    }

    #[test]
    fn split_frontmatter_handles_crlf_line_endings() {
        let raw = "---\r\nname: test\r\ndescription: x\r\n---\r\nbody\r\n";
        let (front, body) = split_frontmatter(raw).expect("parse");
        assert!(front.contains("name: test"));
        assert!(body.starts_with("body"));
    }

    /// PR 3 of `docs/skills_2026-05-02/`. Pin that the four
    /// initial bundled skills are embedded at compile time
    /// and parse cleanly. Embedding (vs. CARGO_MANIFEST_DIR
    /// runtime resolution) means the bundled set works in
    /// release binaries too.
    #[test]
    fn bundled_skills_embedded_and_parse_cleanly() {
        let bundled = bundled_skills();
        let names: Vec<&str> = bundled.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "cpp-rule-of-five",
            "decompose-multi-constraint-task",
            "use-recurse-for-archive-lookup",
            "verify-after-edit",
        ] {
            assert!(
                names.contains(&expected),
                "bundled skill `{expected}` missing; got {names:?}"
            );
        }
        // All four parse without panicking (parse_manifest_from_text
        // would have panicked in `bundled_skills` if any frontmatter
        // were malformed).
        for skill in &bundled {
            assert_eq!(skill.source, SkillSource::Bundled);
            assert!(!skill.body.is_empty());
            assert!(!skill.disable_model_invocation);
        }
    }

    #[test]
    fn registry_precedence_order_within_project_tier() {
        // A skill named the same in all three project layers
        // should resolve to the .anie/ version.
        let cwd_dir = tempdir().expect("cwd tempdir");
        let claude_root = cwd_dir.path().join(PROJECT_CLAUDE_SKILLS_DIR);
        let shared_root = cwd_dir.path().join(PROJECT_SHARED_SKILLS_DIR);
        let anie_root = cwd_dir.path().join(PROJECT_ANIE_SKILLS_DIR);
        write_skill(
            &claude_root,
            "duplicate.md",
            &sample_skill("duplicate", "from claude"),
        );
        write_skill(
            &shared_root,
            "duplicate.md",
            &sample_skill("duplicate", "from agents"),
        );
        write_skill(
            &anie_root,
            "duplicate.md",
            &sample_skill("duplicate", "from anie"),
        );

        let mut registry = SkillRegistry::empty();
        // Insert in lowest-precedence-first order, matching
        // the production discovery loop.
        registry.absorb_root(&claude_root, SkillSource::ProjectClaude);
        registry.absorb_root(&shared_root, SkillSource::ProjectShared);
        registry.absorb_root(&anie_root, SkillSource::ProjectAnie);

        let skill = registry.get("duplicate").expect("present");
        assert_eq!(skill.source, SkillSource::ProjectAnie);
        assert_eq!(skill.description, "from anie");
    }

    #[test]
    fn registry_loads_skills_from_project_claude_layer() {
        let cwd_dir = tempdir().expect("cwd tempdir");
        let claude_root = cwd_dir.path().join(PROJECT_CLAUDE_SKILLS_DIR);
        write_skill(
            &claude_root,
            "claude-only.md",
            &sample_skill("claude-only", "Lives only under .claude/"),
        );

        let mut registry = SkillRegistry::empty();
        registry.absorb_root(&claude_root, SkillSource::ProjectClaude);
        let skill = registry.get("claude-only").expect("present");
        assert_eq!(skill.source, SkillSource::ProjectClaude);
        assert_eq!(skill.source.label(), "project-claude");
    }

    /// `discover` picks up bundled skills even with no on-
    /// disk roots present. Critical: this is the path
    /// production binaries take.
    #[test]
    fn discover_includes_bundled_when_no_disk_roots() {
        let dir = tempdir().expect("tempdir");
        // cwd has no .anie/skills or .agents/skills; home_dir
        // probably exists in test env but won't have our skills.
        let registry = SkillRegistry::discover(dir.path());
        let names: Vec<String> = registry.iter().map(|s| s.name.clone()).collect();
        assert!(
            names.contains(&"cpp-rule-of-five".to_string()),
            "bundled cpp-rule-of-five missing from discover() result; got {names:?}"
        );
    }

    #[test]
    fn split_frontmatter_strips_bom() {
        let raw = "\u{feff}---\nname: test\ndescription: x\n---\nbody\n";
        let (front, body) = split_frontmatter(raw).expect("parse");
        assert!(front.contains("name: test"));
        assert!(body.starts_with("body"));
    }
}
