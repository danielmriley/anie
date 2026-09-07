//! Repo-map builder: a token-capped repo skeleton (file tree +
//! regex signature skeleton + recent git history) injected at the
//! first model turn so a small model can navigate without burning
//! turns on exploratory `find`/`ls` calls.
//!
//! Plan 02 §2a of `docs/local_model_augmentation/`. The plan
//! places this module in anie-cli; it lives here because the
//! `ignore` + `regex` deps are already in anie-tools (anie-cli
//! carries neither) and the drill-down tool's helpers
//! ([`extract_signatures`], [`dir_overview`]) belong beside the
//! builder. anie-cli owns the policy / tool / caching wrappers.
//!
//! Regex header extraction is anie-specific (deviation from
//! Aider-style tree-sitter maps): zero new dependencies, good
//! enough to point `read` at the right region. Tree-sitter is
//! explicitly deferred.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

use ignore::WalkBuilder;
use regex::Regex;

/// Walker entry cap — same precedent as `find`'s 1000-result cap;
/// past this the map degrades rather than stalling on huge repos.
const MAX_WALK_ENTRIES: usize = 5_000;
/// Directories with more direct entries than this collapse to a
/// single `name/ … (N files)` line in the tree.
const COLLAPSE_DIR_ENTRIES: usize = 20;
/// Bound on files scanned for the signature section (largest +
/// most recently modified first).
const MAX_SIGNATURE_FILES: usize = 50;
/// Files bigger than this are skipped by the signature scanner.
const MAX_SIGNATURE_FILE_BYTES: u64 = 512 * 1024;
/// Per-line render cap so one pathological signature line can't
/// eat the budget.
const MAX_SIGNATURE_LINE_CHARS: usize = 160;

/// A rendered repo map plus the (path, mtime) stamp of the walked
/// file set, for staleness checks (the `SystemPromptCache`
/// pattern: recompute and compare, so additions, deletions, and
/// modifications are all detected).
#[derive(Debug, Clone)]
pub struct RepoMap {
    /// Rendered, token-capped markdown skeleton.
    pub text: String,
    stamp: Vec<(PathBuf, Option<SystemTime>)>,
}

impl RepoMap {
    /// True when any walked file was modified, added, or deleted
    /// since this map was built.
    #[must_use]
    pub fn is_stale(&self, cwd: &Path) -> bool {
        stamp_of(&walk_files(cwd)) != self.stamp
    }
}

/// One file the walker found, relative to the walk root.
struct WalkedFile {
    rel_path: PathBuf,
    mtime: Option<SystemTime>,
    size: u64,
}

/// Build the repo map for `cwd` under a token cap (measured with
/// the same bytes/4 heuristic as `anie_session::estimate_tokens`'
/// text rule). Content in priority order until the cap: file
/// tree, then signature skeleton, then recent git history —
/// assembly stops at the first line that doesn't fit, so the
/// lowest-priority sections drop first.
#[must_use]
pub fn build_repo_map(cwd: &Path, max_tokens: u64) -> RepoMap {
    let files = walk_files(cwd);
    let stamp = stamp_of(&files);

    let mut out = LineBudget::new(max_tokens);
    out.push_section("## Files", &render_tree(&files));
    out.push_section("## Symbols", &render_signatures(cwd, &files));
    out.push_section("## Recent commits", &git_log_oneline(cwd));

    RepoMap {
        text: out.into_text(),
        stamp,
    }
}

/// Greedy line accumulator: lines append while the rendered text
/// stays under the token cap, then everything else is dropped.
struct LineBudget {
    lines: Vec<String>,
    bytes: usize,
    max_tokens: u64,
    exhausted: bool,
}

impl LineBudget {
    fn new(max_tokens: u64) -> Self {
        Self {
            lines: Vec::new(),
            bytes: 0,
            max_tokens,
            exhausted: false,
        }
    }

    fn try_push(&mut self, line: String) -> bool {
        if self.exhausted {
            return false;
        }
        let added = line.len() + 1; // trailing newline on join
        if ((self.bytes + added) / 4) as u64 > self.max_tokens {
            self.exhausted = true;
            return false;
        }
        self.bytes += added;
        self.lines.push(line);
        true
    }

    /// Append a header + body lines; the header only lands when at
    /// least the first body line also fits (no orphan headers).
    fn push_section(&mut self, header: &str, body: &[String]) {
        if self.exhausted || body.is_empty() {
            return;
        }
        let Some(first) = body.first() else { return };
        let added = header.len() + 1 + first.len() + 1;
        if ((self.bytes + added) / 4) as u64 > self.max_tokens {
            self.exhausted = true;
            return;
        }
        if !self.lines.is_empty() {
            self.try_push(String::new());
        }
        self.try_push(header.to_string());
        for line in body {
            if !self.try_push(line.clone()) {
                return;
            }
        }
    }

    fn into_text(self) -> String {
        self.lines.join("\n")
    }
}

/// Walk `cwd` with the same gitignore-aware walker `find` uses
/// (hidden files skipped, `.gitignore` respected inside a git
/// repo), capped at [`MAX_WALK_ENTRIES`] files, sorted by path
/// for deterministic output.
fn walk_files(cwd: &Path) -> Vec<WalkedFile> {
    let mut files = Vec::new();
    for entry in WalkBuilder::new(cwd).build().flatten() {
        if files.len() >= MAX_WALK_ENTRIES {
            break;
        }
        let is_file = entry.file_type().is_some_and(|t| t.is_file());
        if !is_file {
            continue;
        }
        let Ok(rel_path) = entry.path().strip_prefix(cwd) else {
            continue;
        };
        let metadata = entry.metadata().ok();
        files.push(WalkedFile {
            rel_path: rel_path.to_path_buf(),
            mtime: metadata.as_ref().and_then(|m| m.modified().ok()),
            size: metadata.as_ref().map_or(0, std::fs::Metadata::len),
        });
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    files
}

fn stamp_of(files: &[WalkedFile]) -> Vec<(PathBuf, Option<SystemTime>)> {
    files
        .iter()
        .map(|f| (f.rel_path.clone(), f.mtime))
        .collect()
}

#[derive(Default)]
struct DirNode {
    dirs: BTreeMap<String, DirNode>,
    files: Vec<String>,
}

impl DirNode {
    fn insert(&mut self, rel_path: &Path) {
        let mut node = self;
        let components: Vec<_> = rel_path.components().collect();
        for (idx, component) in components.iter().enumerate() {
            let name = component.as_os_str().to_string_lossy().into_owned();
            if idx + 1 == components.len() {
                node.files.push(name);
            } else {
                node = node.dirs.entry(name).or_default();
            }
        }
    }

    fn total_files(&self) -> usize {
        self.files.len() + self.dirs.values().map(DirNode::total_files).sum::<usize>()
    }

    fn render(&self, indent: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(indent);
        for (name, dir) in &self.dirs {
            let entries = dir.dirs.len() + dir.files.len();
            if entries > COLLAPSE_DIR_ENTRIES {
                out.push(format!("{pad}{name}/ … ({} files)", dir.total_files()));
            } else {
                out.push(format!("{pad}{name}/"));
                dir.render(indent + 1, out);
            }
        }
        for file in &self.files {
            out.push(format!("{pad}{file}"));
        }
    }
}

fn render_tree(files: &[WalkedFile]) -> Vec<String> {
    let mut root = DirNode::default();
    for file in files {
        root.insert(&file.rel_path);
    }
    let mut out = Vec::new();
    root.render(0, &mut out);
    out
}

/// Languages the regex skeleton understands.
#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Python,
    TsJs,
}

fn lang_for(path: &Path) -> Option<Lang> {
    match path.extension()?.to_str()? {
        "rs" => Some(Lang::Rust),
        "py" => Some(Lang::Python),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => Some(Lang::TsJs),
        _ => None,
    }
}

// Top-level declaration headers only (column 0): methods inside
// impl/class bodies stay out of the breadth-capped map; the
// drill-down tool's per-file mode is where depth lives. `Option`
// (not `expect`) per the workspace's expect_used lint; the
// regression test below pins all three as compilable.
static RUST_SIG: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"^(?:pub(?:\([^)]*\))?\s+(?:async\s+)?(?:unsafe\s+)?(?:fn|struct|enum|trait)\b|impl\b)",
    )
    .ok()
});
static PYTHON_SIG: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^(?:async\s+)?def\s+\w+|^class\s+\w+").ok());
static TS_JS_SIG: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s+\w+|^(?:export\s+)?(?:abstract\s+)?class\s+\w+")
        .ok()
});

fn sig_regex(lang: Lang) -> Option<&'static Regex> {
    match lang {
        Lang::Rust => RUST_SIG.as_ref(),
        Lang::Python => PYTHON_SIG.as_ref(),
        Lang::TsJs => TS_JS_SIG.as_ref(),
    }
}

/// Extract `(1-based line, signature)` pairs for one file's
/// top-level declarations. Empty for unknown languages, unreadable
/// files, and files over [`MAX_SIGNATURE_FILE_BYTES`].
#[must_use]
pub fn extract_signatures(path: &Path) -> Vec<(usize, String)> {
    let Some(lang) = lang_for(path) else {
        return Vec::new();
    };
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_SIGNATURE_FILE_BYTES) {
        return Vec::new();
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Some(regex) = sig_regex(lang) else {
        return Vec::new();
    };
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(idx, line)| {
            let mut sig = line.trim_end().trim_end_matches('{').trim_end().to_string();
            if sig.len() > MAX_SIGNATURE_LINE_CHARS {
                sig.truncate(
                    (0..=MAX_SIGNATURE_LINE_CHARS)
                        .rev()
                        .find(|&i| sig.is_char_boundary(i))
                        .unwrap_or(0),
                );
                sig.push('…');
            }
            (idx + 1, sig)
        })
        .collect()
}

/// Signature skeleton lines, `path:line: sig`, for the most
/// relevant files first (most recently modified, then largest).
fn render_signatures(cwd: &Path, files: &[WalkedFile]) -> Vec<String> {
    let mut candidates: Vec<&WalkedFile> = files
        .iter()
        .filter(|f| lang_for(&f.rel_path).is_some())
        .collect();
    candidates.sort_by(|a, b| {
        b.mtime
            .cmp(&a.mtime)
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    let mut out = Vec::new();
    for file in candidates.into_iter().take(MAX_SIGNATURE_FILES) {
        let rel = file.rel_path.display();
        for (line, sig) in extract_signatures(&cwd.join(&file.rel_path)) {
            out.push(format!("{rel}:{line}: {sig}"));
        }
    }
    out
}

/// `git log --oneline -10` for cheap orientation. Empty (section
/// omitted) when `cwd` isn't a repo or git isn't available.
fn git_log_oneline(cwd: &Path) -> Vec<String> {
    let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["log", "--oneline", "-10"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Drill-down view for a directory: its file tree plus per-file
/// symbol counts for recognized source files.
#[must_use]
pub fn dir_overview(cwd: &Path, dir: &Path) -> String {
    let files = walk_files(dir);
    let mut root = DirNode::default();
    for file in &files {
        root.insert(&file.rel_path);
    }
    let mut tree = Vec::new();
    root.render(0, &mut tree);

    let mut counts = Vec::new();
    for file in &files {
        if lang_for(&file.rel_path).is_none() {
            continue;
        }
        let n = extract_signatures(&dir.join(&file.rel_path)).len();
        if n > 0 {
            counts.push(format!("{}: {n} symbols", file.rel_path.display()));
        }
    }

    let rel = dir.strip_prefix(cwd).unwrap_or(dir);
    let mut out = vec![format!("{}/", rel.display())];
    out.extend(tree.into_iter().map(|line| format!("  {line}")));
    if !counts.is_empty() {
        out.push(String::new());
        out.push("Symbol counts:".to_string());
        out.extend(counts);
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use tempfile::tempdir;

    use super::*;

    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(status.status.success(), "git {args:?} failed");
    }

    fn git_repo_fixture() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(
            root.join("src/lib.rs"),
            "pub fn alpha() {}\npub struct Beta;\nimpl Beta {}\n",
        )
        .expect("write rs");
        fs::write(
            root.join("tool.py"),
            "def gamma():\n    pass\nclass Delta:\n    pass\n",
        )
        .expect("write py");
        fs::write(
            root.join("app.ts"),
            "export function epsilon() {}\nclass Zeta {}\n",
        )
        .expect("write ts");
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "initial skeleton commit"]);
        dir
    }

    #[test]
    fn map_respects_token_cap_and_drops_lowest_priority_sections_first() {
        let dir = git_repo_fixture();
        let full = build_repo_map(dir.path(), 100_000);
        assert!(full.text.contains("## Files"), "{}", full.text);
        assert!(full.text.contains("## Symbols"), "{}", full.text);
        assert!(full.text.contains("## Recent commits"), "{}", full.text);
        assert!(
            full.text.contains("initial skeleton commit"),
            "{}",
            full.text
        );
        assert!(
            (full.text.len() / 4) as u64 <= 100_000,
            "rendered text must respect the cap"
        );

        // A cap that fits the tree but not the signature skeleton:
        // history (lowest priority) and symbols both drop; the tree
        // survives.
        let tree_only = build_repo_map(dir.path(), 30);
        assert!(tree_only.text.contains("## Files"), "{}", tree_only.text);
        assert!(
            !tree_only.text.contains("## Recent commits"),
            "history must drop before the tree: {}",
            tree_only.text
        );
        assert!(
            (tree_only.text.len() / 4) as u64 <= 30,
            "capped text must stay under the cap: {} bytes",
            tree_only.text.len()
        );
    }

    #[test]
    fn rust_python_ts_signatures_extracted_with_path_line_prefixes() {
        let dir = git_repo_fixture();
        let map = build_repo_map(dir.path(), 100_000);
        let rs_path = Path::new("src").join("lib.rs");
        assert!(
            map.text
                .contains(&format!("{}:1: pub fn alpha()", rs_path.display())),
            "{}",
            map.text
        );
        assert!(
            map.text
                .contains(&format!("{}:2: pub struct Beta;", rs_path.display())),
            "{}",
            map.text
        );
        assert!(map.text.contains("tool.py:1: def gamma():"), "{}", map.text);
        assert!(map.text.contains("tool.py:3: class Delta:"), "{}", map.text);
        assert!(
            map.text.contains("app.ts:1: export function epsilon()"),
            "{}",
            map.text
        );
        assert!(map.text.contains("app.ts:2: class Zeta"), "{}", map.text);
    }

    #[test]
    fn gitignored_and_hidden_files_are_excluded_from_the_tree() {
        let dir = git_repo_fixture();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "generated.txt\n").expect("write gitignore");
        fs::write(root.join("generated.txt"), "x").expect("write ignored");
        fs::write(root.join(".hidden_notes"), "x").expect("write hidden");

        let map = build_repo_map(root, 100_000);
        assert!(!map.text.contains("generated.txt"), "{}", map.text);
        assert!(!map.text.contains(".hidden_notes"), "{}", map.text);
        assert!(map.text.contains("tool.py"), "{}", map.text);
    }

    #[test]
    fn oversized_directories_collapse_to_a_count_line() {
        let dir = git_repo_fixture();
        let big = dir.path().join("big");
        fs::create_dir_all(&big).expect("mkdir");
        for i in 0..25 {
            fs::write(big.join(format!("file{i:02}.txt")), "x").expect("write");
        }
        let map = build_repo_map(dir.path(), 100_000);
        assert!(map.text.contains("big/ … (25 files)"), "{}", map.text);
        assert!(
            !map.text.contains("file00.txt"),
            "collapsed dirs must not list entries: {}",
            map.text
        );
    }

    #[test]
    fn stamp_detects_modified_added_and_deleted_files() {
        let dir = git_repo_fixture();
        let root = dir.path();

        let map = build_repo_map(root, 100_000);
        assert!(!map.is_stale(root), "fresh map must not be stale");

        // Modified.
        thread::sleep(Duration::from_millis(10));
        fs::write(root.join("tool.py"), "def gamma_two():\n    pass\n").expect("rewrite");
        assert!(map.is_stale(root), "modified file must stale the map");

        // Added.
        let map = build_repo_map(root, 100_000);
        fs::write(root.join("brand_new.rs"), "pub fn added() {}\n").expect("add");
        assert!(map.is_stale(root), "added file must stale the map");

        // Deleted.
        let map = build_repo_map(root, 100_000);
        fs::remove_file(root.join("brand_new.rs")).expect("remove");
        assert!(map.is_stale(root), "deleted file must stale the map");
    }

    #[test]
    fn non_git_directory_omits_history_section_without_error() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("solo.py"), "def solo():\n    pass\n").expect("write");
        let map = build_repo_map(dir.path(), 100_000);
        assert!(map.text.contains("solo.py"), "{}", map.text);
        assert!(!map.text.contains("## Recent commits"), "{}", map.text);
    }

    // Guards the `.ok()` static-regex pattern above: a typo in a
    // pattern would otherwise silently disable extraction for
    // that whole language.
    #[test]
    fn all_signature_regexes_compile() {
        assert!(sig_regex(Lang::Rust).is_some());
        assert!(sig_regex(Lang::Python).is_some());
        assert!(sig_regex(Lang::TsJs).is_some());
    }

    #[test]
    fn dir_overview_lists_tree_and_symbol_counts() {
        let dir = git_repo_fixture();
        let overview = dir_overview(dir.path(), &dir.path().join("src"));
        assert!(overview.contains("lib.rs"), "{overview}");
        assert!(overview.contains("lib.rs: 3 symbols"), "{overview}");
    }
}
