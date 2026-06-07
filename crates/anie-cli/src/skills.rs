//! Skill discovery and `SKILL.md` frontmatter parsing.
//!
//! A *skill* is a `SKILL.md` file living in a subdirectory of a skills
//! root (`~/.anie/skills/<dir>/SKILL.md` globally, or
//! `.anie/skills/<dir>/SKILL.md` in a project). The file begins with a
//! fenced YAML frontmatter block followed by a free-form Markdown body:
//!
//! ```text
//! ---
//! name: deploy
//! description: Deploy the service to staging
//! allowed-tools: [bash, read]
//! ---
//! Run the deploy script and report the result.
//! ```
//!
//! This is a deliberately **thin** loader (a subset of the deferred
//! Plan-10 extension system). It parses only three flat fields —
//! `name`, `description`, and `allowed-tools` — with a hand-rolled
//! parser rather than pulling in a YAML crate for three keys (the
//! workspace has no YAML dependency, and `serde_yaml` is unmaintained
//! upstream). Unknown keys are ignored. `allowed-tools` is parsed and
//! stored but **not** yet enforced at tool-call time; that, plus
//! auto-injection, are deferred (see `docs/skills_loader/README.md` §8).
//!
//! Discovery is best-effort: a malformed `SKILL.md` yields a typed
//! [`SkillLoadError`] that the caller surfaces as a startup warning;
//! the remaining skills still load. Nothing here panics on user files.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use thiserror::Error;

/// A parsed skill ready to register as a `/skill:<name>` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Skill {
    /// Frontmatter `name`; the command is `/skill:<name>`.
    pub name: String,
    /// Frontmatter `description`; shown as the command summary.
    pub description: String,
    /// Frontmatter `allowed-tools` (parsed, not yet enforced).
    pub allowed_tools: Vec<String>,
    /// Everything after the frontmatter fence — injected as context
    /// when the skill is activated.
    pub body: String,
}

/// An owned collection of discovered skills, keyed by name.
#[derive(Debug, Clone, Default)]
pub(crate) struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

impl SkillSet {
    /// Skills in name order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Look up a skill by its (bare) name.
    pub(crate) fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.skills.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Build a set directly from skills, for tests in sibling modules.
    #[cfg(test)]
    pub(crate) fn from_skills(skills: Vec<Skill>) -> Self {
        Self {
            skills: skills
                .into_iter()
                .map(|skill| (skill.name.clone(), skill))
                .collect(),
        }
    }
}

/// Why a single `SKILL.md` failed to load. Loader-local (file IO +
/// parse); deliberately not part of `ProviderError`/`ToolError`, whose
/// taxonomies are reserved for their own domains.
#[derive(Debug, Error)]
pub(crate) enum SkillLoadError {
    #[error("skill file {0} does not start with a `---` frontmatter fence")]
    MissingFrontmatter(PathBuf),
    #[error("skill file {0} has no closing `---` frontmatter fence")]
    UnterminatedFrontmatter(PathBuf),
    #[error("skill file {path} is missing required frontmatter field `{field}`")]
    MissingField { path: PathBuf, field: &'static str },
    #[error(
        "skill file {path} has an invalid `name` ({name:?}): a skill name must be a single \
         token with no whitespace so `/skill:<name>` is invocable"
    )]
    InvalidName { path: PathBuf, name: String },
    #[error("skill name `{name}` is declared by more than one skill in the same root ({path})")]
    DuplicateName { path: PathBuf, name: String },
    #[error("failed to read skill file {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Discover skills from a global root and an optional project root,
/// with project skills shadowing global skills of the same name.
/// Returns the loaded set plus a (possibly empty) list of per-file
/// load errors; a missing root directory is not an error.
pub(crate) fn discover_skills(
    global_dir: &Path,
    project_dir: Option<&Path>,
) -> (SkillSet, Vec<SkillLoadError>) {
    let mut skills = BTreeMap::new();
    let mut errors = Vec::new();
    // Global first, then project — a project skill overwrites a global
    // skill of the same name.
    load_dir(global_dir, &mut skills, &mut errors);
    if let Some(project_dir) = project_dir {
        load_dir(project_dir, &mut skills, &mut errors);
    }
    (SkillSet { skills }, errors)
}

/// Scan one skills root: each immediate subdirectory containing a
/// `SKILL.md` contributes a skill. Subdirectories without one are
/// skipped; a missing or unreadable root is silently ignored (roots
/// are optional).
fn load_dir(dir: &Path, skills: &mut BTreeMap<String, Skill>, errors: &mut Vec<SkillLoadError>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    // Names declared in *this* root. A clash here is a same-root
    // collision (two dirs declaring `name: deploy`) and is surfaced as
    // an error; a clash against an earlier root is intentional
    // project-over-global shadowing and stays silent.
    let mut seen_in_root: std::collections::HashSet<String> = std::collections::HashSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match parse_skill(&skill_md) {
            Ok(skill) => {
                if !seen_in_root.insert(skill.name.clone()) {
                    errors.push(SkillLoadError::DuplicateName {
                        path: skill_md,
                        name: skill.name,
                    });
                    continue;
                }
                skills.insert(skill.name.clone(), skill);
            }
            Err(error) => errors.push(error),
        }
    }
}

/// Parse a single `SKILL.md`. See the module header for the supported
/// frontmatter subset.
pub(crate) fn parse_skill(path: &Path) -> Result<Skill, SkillLoadError> {
    let content = fs::read_to_string(path).map_err(|source| SkillLoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    // Strip a leading UTF-8 BOM if present so the fence check is clean.
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let lines: Vec<&str> = content.lines().collect();

    let Some(open) = lines.iter().position(|line| !line.trim().is_empty()) else {
        return Err(SkillLoadError::MissingFrontmatter(path.to_path_buf()));
    };
    if lines[open].trim() != "---" {
        return Err(SkillLoadError::MissingFrontmatter(path.to_path_buf()));
    }
    let Some(close_offset) = lines[open + 1..]
        .iter()
        .position(|line| line.trim() == "---")
    else {
        return Err(SkillLoadError::UnterminatedFrontmatter(path.to_path_buf()));
    };
    let close = open + 1 + close_offset;
    let frontmatter = &lines[open + 1..close];
    let body = lines[close + 1..].join("\n").trim().to_string();

    let mut name = None;
    let mut description = None;
    let mut allowed_tools = Vec::new();

    let mut i = 0;
    while i < frontmatter.len() {
        let line = frontmatter[i];
        let Some((key, value)) = line.split_once(':') else {
            i += 1;
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim());
        match key {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            "allowed-tools" => {
                if value.is_empty() {
                    // Block list: consecutive `- item` lines follow.
                    let mut j = i + 1;
                    while j < frontmatter.len() {
                        let trimmed = frontmatter[j].trim();
                        let Some(item) = trimmed.strip_prefix("- ") else {
                            break;
                        };
                        let item = unquote(item.trim());
                        if !item.is_empty() {
                            allowed_tools.push(item.to_string());
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                } else if let Some(inner) =
                    value.strip_prefix('[').and_then(|v| v.strip_suffix(']'))
                {
                    // Inline flow list `[a, b, c]`.
                    for item in inner.split(',') {
                        let item = unquote(item.trim());
                        if !item.is_empty() {
                            allowed_tools.push(item.to_string());
                        }
                    }
                } else {
                    allowed_tools.push(value.to_string());
                }
            }
            _ => {} // Unknown keys are ignored.
        }
        i += 1;
    }

    let name = name
        .filter(|value| !value.is_empty())
        .ok_or(SkillLoadError::MissingField {
            path: path.to_path_buf(),
            field: "name",
        })?;
    // The command is `/skill:<name>`, and slash-command dispatch splits
    // the input on the first whitespace — a name with a space would
    // produce a permanently uninvokable command. Reject it loudly.
    if name.chars().any(char::is_whitespace) {
        return Err(SkillLoadError::InvalidName {
            path: path.to_path_buf(),
            name,
        });
    }
    let description =
        description
            .filter(|value| !value.is_empty())
            .ok_or(SkillLoadError::MissingField {
                path: path.to_path_buf(),
                field: "description",
            })?;

    Ok(Skill {
        name,
        description,
        allowed_tools,
        body,
    })
}

/// Strip a single layer of matching surrounding single or double
/// quotes from a frontmatter value.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, dir: &str, contents: &str) {
        let skill_dir = root.join(dir);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(skill_dir.join("SKILL.md"), contents).unwrap();
    }

    #[test]
    fn parses_minimal_frontmatter_name_and_description() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "deploy",
            "---\nname: deploy\ndescription: Deploy it\n---\nThe body.\n",
        );
        let skill = parse_skill(&dir.path().join("deploy/SKILL.md")).unwrap();
        assert_eq!(skill.name, "deploy");
        assert_eq!(skill.description, "Deploy it");
        assert_eq!(skill.body, "The body.");
        assert!(skill.allowed_tools.is_empty());
    }

    #[test]
    fn parses_allowed_tools_inline_flow_list() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "s",
            "---\nname: s\ndescription: d\nallowed-tools: [bash, read, \"edit\"]\n---\nbody\n",
        );
        let skill = parse_skill(&dir.path().join("s/SKILL.md")).unwrap();
        assert_eq!(skill.allowed_tools, vec!["bash", "read", "edit"]);
    }

    #[test]
    fn parses_allowed_tools_block_list() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "s",
            "---\nname: s\ndescription: d\nallowed-tools:\n  - bash\n  - read\n---\nbody\n",
        );
        let skill = parse_skill(&dir.path().join("s/SKILL.md")).unwrap();
        assert_eq!(skill.allowed_tools, vec!["bash", "read"]);
    }

    #[test]
    fn body_excludes_frontmatter_fence() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "s",
            "---\nname: s\ndescription: d\n---\nline one\nline two\n",
        );
        let skill = parse_skill(&dir.path().join("s/SKILL.md")).unwrap();
        assert_eq!(skill.body, "line one\nline two");
        assert!(!skill.body.contains("---"));
    }

    #[test]
    fn whitespace_in_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "s",
            "---\nname: my skill\ndescription: d\n---\nbody\n",
        );
        let err = parse_skill(&dir.path().join("s/SKILL.md")).unwrap_err();
        assert!(
            matches!(&err, SkillLoadError::InvalidName { name, .. } if name == "my skill"),
            "{err:?}"
        );
    }

    #[test]
    fn same_root_duplicate_name_surfaces_an_error_not_silent_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "deploy-staging",
            "---\nname: deploy\ndescription: staging\n---\nbody\n",
        );
        write_skill(
            dir.path(),
            "deploy-prod",
            "---\nname: deploy\ndescription: prod\n---\nbody\n",
        );
        let (set, errors) = discover_skills(dir.path(), None);
        // Exactly one `deploy` survives, and the collision is reported.
        assert_eq!(set.len(), 1);
        assert!(set.get("deploy").is_some());
        assert!(
            errors.iter().any(
                |e| matches!(e, SkillLoadError::DuplicateName { name, .. } if name == "deploy")
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn cross_root_same_name_still_shadows_silently() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(
            global.path(),
            "dup",
            "---\nname: dup\ndescription: global\n---\nbody\n",
        );
        write_skill(
            project.path(),
            "dup",
            "---\nname: dup\ndescription: project\n---\nbody\n",
        );
        let (set, errors) = discover_skills(global.path(), Some(project.path()));
        // Project wins; no DuplicateName error (cross-root shadow is intentional).
        assert_eq!(set.get("dup").unwrap().description, "project");
        assert!(
            !errors
                .iter()
                .any(|e| matches!(e, SkillLoadError::DuplicateName { .. })),
            "cross-root shadowing must stay silent: {errors:?}"
        );
    }

    #[test]
    fn missing_name_is_skill_load_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "s", "---\ndescription: d\n---\nbody\n");
        let err = parse_skill(&dir.path().join("s/SKILL.md")).unwrap_err();
        assert!(matches!(
            err,
            SkillLoadError::MissingField { field: "name", .. }
        ));
    }

    #[test]
    fn missing_closing_fence_is_skill_load_error() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "s",
            "---\nname: s\ndescription: d\nbody with no close\n",
        );
        let err = parse_skill(&dir.path().join("s/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillLoadError::UnterminatedFrontmatter(_)));
    }

    #[test]
    fn missing_opening_fence_is_skill_load_error() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "s", "name: s\ndescription: d\n");
        let err = parse_skill(&dir.path().join("s/SKILL.md")).unwrap_err();
        assert!(matches!(err, SkillLoadError::MissingFrontmatter(_)));
    }

    #[test]
    fn unknown_frontmatter_keys_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "s",
            "---\nname: s\ndescription: d\nlicense: MIT\nmetadata: whatever\n---\nbody\n",
        );
        let skill = parse_skill(&dir.path().join("s/SKILL.md")).unwrap();
        assert_eq!(skill.name, "s");
        assert!(skill.allowed_tools.is_empty());
    }

    #[test]
    fn project_skill_shadows_global_skill_of_same_name() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write_skill(
            global.path(),
            "dup",
            "---\nname: dup\ndescription: global one\n---\nglobal body\n",
        );
        write_skill(
            project.path(),
            "dup",
            "---\nname: dup\ndescription: project one\n---\nproject body\n",
        );
        let (set, errors) = discover_skills(global.path(), Some(project.path()));
        assert!(errors.is_empty());
        assert_eq!(set.len(), 1);
        assert_eq!(set.get("dup").unwrap().description, "project one");
    }

    #[test]
    fn discovery_skips_dirs_without_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("not-a-skill")).unwrap();
        fs::write(dir.path().join("not-a-skill/README.md"), "nope").unwrap();
        write_skill(
            dir.path(),
            "real",
            "---\nname: real\ndescription: d\n---\nbody\n",
        );
        let (set, errors) = discover_skills(dir.path(), None);
        assert!(errors.is_empty());
        assert_eq!(set.len(), 1);
        assert!(set.get("real").is_some());
    }

    #[test]
    fn one_malformed_skill_does_not_drop_the_others() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(
            dir.path(),
            "good",
            "---\nname: good\ndescription: d\n---\nbody\n",
        );
        write_skill(dir.path(), "bad", "no frontmatter here\n");
        let (set, errors) = discover_skills(dir.path(), None);
        assert_eq!(set.len(), 1, "the good skill still loads");
        assert!(set.get("good").is_some());
        assert_eq!(errors.len(), 1, "the bad skill yields one error");
    }

    #[test]
    fn missing_root_dir_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (set, errors) = discover_skills(&dir.path().join("does-not-exist"), None);
        assert!(set.is_empty());
        assert!(errors.is_empty());
    }
}
