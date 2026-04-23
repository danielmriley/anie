//! Content-search tool backed by ripgrep's libraries.
//!
//! Uses `grep-searcher` + `grep-regex` + `ignore` directly rather
//! than shelling out to `rg`. Same algorithmic behavior as ripgrep
//! (the CLI is largely a wrapper over these crates), no PATH
//! dependency, no version skew across platforms.
//!
//! Parameter names match pi's grep tool shape (pi:
//! `packages/coding-agent/src/core/tools/grep.ts`): `ignoreCase`,
//! `literal`, `context`. Defaults match pi: 100 matches + 50 KB
//! byte cap.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::{WalkBuilder, types::TypesBuilder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::shared::{resolve_path, text_result};

/// Match / byte limits matching pi's defaults so prompting that
/// references pi's grep behavior transfers directly.
const DEFAULT_MATCH_LIMIT: usize = 100;
const BYTE_LIMIT: usize = 50 * 1024;
/// Cap on any single line of output so a file with one absurdly
/// long line can't single-handedly blow the byte budget.
const LINE_CHAR_CAP: usize = 500;

/// Search file contents for a pattern.
pub struct GrepTool {
    cwd: Arc<PathBuf>,
}

impl GrepTool {
    /// Create a grep tool rooted at the provided working directory.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self {
            cwd: Arc::new(cwd.into()),
        }
    }

    fn resolve_path(&self, path: Option<&str>) -> PathBuf {
        match path {
            Some(p) => resolve_path(self.cwd.as_ref(), p),
            None => self.cwd.as_ref().clone(),
        }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grep".into(),
            description: "Search file contents for a regex pattern. Respects .gitignore by default. Returns `path:line:match` triples, up to 100 matches or 50 KB of output (whichever hits first). Use `ignoreCase` for case-insensitive matching, `literal` to treat the pattern as a fixed string instead of a regex, and `context` to include N lines before/after each match.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regex pattern to search for (unless `literal: true`)."
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search. Defaults to the session cwd."
                    },
                    "glob": {
                        "type": "string",
                        "description": "Glob filter (e.g. '*.rs', 'src/**/*.ts')."
                    },
                    "type": {
                        "type": "string",
                        "description": "File-type filter (e.g. 'rust', 'js', 'py'). See ripgrep's --type list."
                    },
                    "ignoreCase": {
                        "type": "boolean",
                        "description": "Case-insensitive match (default false)."
                    },
                    "literal": {
                        "type": "boolean",
                        "description": "Treat the pattern as a literal string (default false)."
                    },
                    "context": {
                        "type": "integer",
                        "description": "Lines of context before and after each match (default 0)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of matches to return (default 100)."
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<anie_protocol::ToolResult>>,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let options = GrepOptions::from_args(&args)?;
        let search_root = self.resolve_path(options.path.as_deref());
        let cwd = Arc::clone(&self.cwd);

        // grep-searcher + ignore are sync; run them on a blocking
        // thread so we don't tie up the tokio worker. The cancel
        // token is honored both inside the sink (fast-path) and
        // by the outer await.
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag_for_watcher = Arc::clone(&cancel_flag);
        let cancel_watcher = cancel.clone();
        let watcher_handle = tokio::spawn(async move {
            cancel_watcher.cancelled().await;
            cancel_flag_for_watcher.store(true, Ordering::Relaxed);
        });

        let result = tokio::task::spawn_blocking(move || {
            run_search(&cwd, &search_root, &options, &cancel_flag)
        })
        .await
        .map_err(|error| {
            ToolError::ExecutionFailed(format!("grep task join error: {error}"))
        })??;
        watcher_handle.abort();

        let details = serde_json::json!({
            "pattern": result.pattern,
            "match_count": result.match_count,
            "file_count": result.files_with_matches,
            "truncated": result.truncated_reason.is_some(),
        });
        let body = if result.match_count == 0 {
            "No matches.".to_string()
        } else {
            let mut out = result.output;
            if let Some(reason) = result.truncated_reason {
                out.push_str("\n[Truncated: ");
                out.push_str(&reason);
                out.push_str("]\n");
            }
            out
        };
        Ok(text_result(body, details))
    }
}

#[derive(Debug)]
struct GrepOptions {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    file_type: Option<String>,
    ignore_case: bool,
    literal: bool,
    context: usize,
    limit: usize,
}

impl GrepOptions {
    fn from_args(args: &serde_json::Value) -> Result<Self, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::ExecutionFailed("Missing 'pattern' argument".into()))?
            .to_string();
        if pattern.is_empty() {
            return Err(ToolError::ExecutionFailed(
                "'pattern' must not be empty".into(),
            ));
        }
        let path = args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let glob = args
            .get("glob")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let file_type = args
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let ignore_case = args
            .get("ignoreCase")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let literal = args
            .get("literal")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let context = args
            .get("context")
            .and_then(serde_json::Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(0))
            .unwrap_or(0);
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(DEFAULT_MATCH_LIMIT))
            .unwrap_or(DEFAULT_MATCH_LIMIT);
        Ok(Self {
            pattern,
            path,
            glob,
            file_type,
            ignore_case,
            literal,
            context,
            limit,
        })
    }
}

struct SearchResult {
    pattern: String,
    output: String,
    match_count: usize,
    files_with_matches: usize,
    truncated_reason: Option<String>,
}

fn run_search(
    cwd: &Path,
    search_root: &Path,
    options: &GrepOptions,
    cancel: &AtomicBool,
) -> Result<SearchResult, ToolError> {
    // Build the pattern: literal → escape, otherwise interpret as
    // a regex. Case-insensitive is a matcher flag, not a pattern
    // transformation, so it works for both.
    let pattern_text = if options.literal {
        regex_escape(&options.pattern)
    } else {
        options.pattern.clone()
    };
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(options.ignore_case)
        .build(&pattern_text)
        .map_err(|error| ToolError::ExecutionFailed(format!("invalid pattern: {error}")))?;

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder
        .before_context(options.context)
        .after_context(options.context)
        .line_number(true);

    let mut walk_builder = WalkBuilder::new(search_root);
    walk_builder
        .standard_filters(true)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true);

    if let Some(file_type) = options.file_type.as_ref() {
        let mut type_builder = TypesBuilder::new();
        type_builder.add_defaults();
        type_builder.select(file_type);
        let types = type_builder
            .build()
            .map_err(|error| ToolError::ExecutionFailed(format!("invalid type filter: {error}")))?;
        walk_builder.types(types);
    }
    if let Some(glob) = options.glob.as_ref() {
        let mut override_builder = ignore::overrides::OverrideBuilder::new(search_root);
        override_builder
            .add(glob)
            .map_err(|error| ToolError::ExecutionFailed(format!("invalid glob: {error}")))?;
        let overrides = override_builder
            .build()
            .map_err(|error| ToolError::ExecutionFailed(format!("glob build failed: {error}")))?;
        walk_builder.overrides(overrides);
    }

    let mut output = String::new();
    let mut match_count = 0usize;
    let mut files_with_matches = 0usize;
    let mut truncated_reason = None;

    'walk: for entry in walk_builder.build() {
        if cancel.load(Ordering::Relaxed) {
            return Err(ToolError::Aborted);
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let display_path = path
            .strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let mut searcher = searcher_builder.build();
        let mut sink = MatchCollector {
            display_path: &display_path,
            output: &mut output,
            match_count: &mut match_count,
            limit: options.limit,
            byte_limit: BYTE_LIMIT,
            truncated_reason: &mut truncated_reason,
            cancel,
            file_had_match: false,
        };

        let search_result = searcher.search_path(&matcher, path, &mut sink);
        if sink.file_had_match {
            files_with_matches += 1;
        }
        if let Err(error) = search_result {
            if error.to_string() == "search aborted" {
                // User cancel or cap hit — stop walking.
                break 'walk;
            }
            // Binary files, permission errors, etc. — skip and
            // continue.
            continue;
        }
        if truncated_reason.is_some() {
            break;
        }
    }

    Ok(SearchResult {
        pattern: options.pattern.clone(),
        output,
        match_count,
        files_with_matches,
        truncated_reason,
    })
}

struct MatchCollector<'a> {
    display_path: &'a str,
    output: &'a mut String,
    match_count: &'a mut usize,
    limit: usize,
    byte_limit: usize,
    truncated_reason: &'a mut Option<String>,
    cancel: &'a AtomicBool,
    file_had_match: bool,
}

impl<'a> MatchCollector<'a> {
    fn append_line(&mut self, prefix: char, line_number: u64, line_bytes: &[u8]) -> bool {
        if self.cancel.load(Ordering::Relaxed) {
            return false;
        }
        let line = String::from_utf8_lossy(line_bytes);
        let line = line.trim_end_matches('\n').trim_end_matches('\r');
        // Plan 07 PR-A+B: use the shared truncation helper —
        // zero-copy `Cow::Borrowed` when the line already
        // fits under LINE_CHAR_CAP, one `String` allocation
        // when truncation applies. No intermediate
        // `truncated_line` String either way.
        let (truncated_line, _truncated) =
            crate::shared::truncate_line_to_chars(line, LINE_CHAR_CAP);

        // Estimate byte cost of the formatted line without
        // actually formatting it. `prefix` is one char
        // (context marker) which fits in a single byte; the
        // path + ":" + line-number + ":" + content + "\n".
        let line_number_len = line_number_digit_count(line_number);
        let addition_len = 1 // prefix
            + self.display_path.len()
            + 1 // colon
            + line_number_len
            + 1 // colon
            + truncated_line.len()
            + 1; // trailing newline

        if crate::shared::would_exceed_byte_limit(
            self.output.len(),
            addition_len,
            self.byte_limit,
        ) {
            if self.truncated_reason.is_none() {
                *self.truncated_reason =
                    Some("50 KB byte limit reached. Narrow the pattern or path.".to_string());
            }
            return false;
        }
        // Plan 07 PR-B: write directly into self.output —
        // the previous shape built a `new_content` String
        // with `format!` then pushed it; we skip that
        // intermediate via `writeln!`.
        use std::fmt::Write as _;
        // Writing into a String never errors; the ignored
        // result is safe here.
        let _ = writeln!(
            self.output,
            "{prefix}{path}:{line_number}:{truncated_line}",
            path = self.display_path,
        );
        true
    }
}

/// Decimal digit count for a `u64`. Used by `append_line` to
/// size the byte-budget estimate without allocating.
fn line_number_digit_count(n: u64) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut value = n;
    while value > 0 {
        count += 1;
        value /= 10;
    }
    count
}

impl<'a> Sink for MatchCollector<'a> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.file_had_match = true;
        let line_number = mat.line_number().unwrap_or(0);
        for line in mat.lines() {
            if !self.append_line(' ', line_number, line) {
                return Ok(false);
            }
        }
        *self.match_count += 1;
        if *self.match_count >= self.limit {
            if self.truncated_reason.is_none() {
                *self.truncated_reason = Some(format!(
                    "{} match limit reached. Use a larger `limit` or narrow the pattern.",
                    self.limit,
                ));
            }
            return Ok(false);
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let prefix = match ctx.kind() {
            SinkContextKind::Before => '-',
            SinkContextKind::After => '+',
            _ => ' ',
        };
        let line_number = ctx.line_number().unwrap_or(0);
        let cont = self.append_line(prefix, line_number, ctx.bytes());
        Ok(cont)
    }
}

/// Escape regex metacharacters so a literal-mode search treats the
/// pattern as a fixed string. We build on top of `grep-regex` which
/// is regex-only, so literal mode = escape + compile.
fn regex_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\'
            | '#' | '-' | '&' | '~' | ':' | '"' | '\'' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_tree(dir: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let full = dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("create parent");
            }
            std::fs::write(&full, content).expect("write fixture");
        }
    }

    async fn run_grep(
        cwd: &Path,
        args: serde_json::Value,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let tool = GrepTool::new(cwd);
        tool.execute("call", args, CancellationToken::new(), None).await
    }

    fn text_body(result: &anie_protocol::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| match block {
                anie_protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn grep_finds_matches_with_path_line_text_format() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(
            tempdir.path(),
            &[
                ("a.txt", "alpha\nbeta\ngamma\n"),
                ("b.txt", "beta\ndelta\n"),
            ],
        );
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "beta" }),
        )
        .await
        .expect("grep");
        let body = text_body(&result);
        assert!(body.contains("a.txt:2:beta"), "{body}");
        assert!(body.contains("b.txt:1:beta"), "{body}");
        assert_eq!(result.details["match_count"], 2);
        assert_eq!(result.details["file_count"], 2);
    }

    #[tokio::test]
    async fn grep_ignore_case_flag() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(tempdir.path(), &[("a.txt", "Beta\nGamma\n")]);
        let strict = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "beta" }),
        )
        .await
        .expect("strict");
        assert_eq!(strict.details["match_count"], 0);
        let loose = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "beta", "ignoreCase": true }),
        )
        .await
        .expect("loose");
        assert_eq!(loose.details["match_count"], 1);
    }

    #[tokio::test]
    async fn grep_literal_flag_treats_pattern_as_fixed_string() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(tempdir.path(), &[("a.txt", "a.b.c\naXb\n")]);
        // `.` is a regex wildcard by default → matches both lines.
        let regex = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "a.b" }),
        )
        .await
        .expect("regex");
        assert_eq!(regex.details["match_count"], 2);
        // Literal → only the line with the literal `a.b` matches.
        let literal = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "a.b", "literal": true }),
        )
        .await
        .expect("literal");
        assert_eq!(literal.details["match_count"], 1);
    }

    #[tokio::test]
    async fn grep_respects_gitignore() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(
            tempdir.path(),
            &[
                (".gitignore", "ignored.txt\n"),
                ("visible.txt", "beta\n"),
                ("ignored.txt", "beta\n"),
            ],
        );
        // Initialize as a git repo so the ignore crate treats
        // .gitignore as a ripgrep-style source of truth.
        std::fs::create_dir_all(tempdir.path().join(".git")).expect("mkdir .git");
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "beta" }),
        )
        .await
        .expect("grep");
        let body = text_body(&result);
        assert!(body.contains("visible.txt"));
        assert!(!body.contains("ignored.txt"));
    }

    #[tokio::test]
    async fn grep_glob_filter() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(
            tempdir.path(),
            &[
                ("src/code.rs", "fn foo() {}\n"),
                ("docs/notes.md", "fn foo() something\n"),
            ],
        );
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "foo", "glob": "*.rs" }),
        )
        .await
        .expect("grep");
        let body = text_body(&result);
        assert!(body.contains("code.rs"));
        assert!(!body.contains("notes.md"));
    }

    #[tokio::test]
    async fn grep_truncates_when_limit_reached() {
        let tempdir = tempdir().expect("tempdir");
        let content = "hit\n".repeat(200);
        make_tree(tempdir.path(), &[("a.txt", content.as_str())]);
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "hit", "limit": 5 }),
        )
        .await
        .expect("grep");
        assert_eq!(result.details["match_count"], 5);
        assert_eq!(result.details["truncated"], true);
        let body = text_body(&result);
        assert!(body.contains("5 match limit reached"), "{body}");
    }

    #[tokio::test]
    async fn grep_includes_context_lines_when_requested() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(
            tempdir.path(),
            &[("a.txt", "before\nmatchline\nafter\n")],
        );
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "matchline", "context": 1 }),
        )
        .await
        .expect("grep");
        let body = text_body(&result);
        assert!(body.contains("-a.txt:1:before"), "{body}");
        assert!(body.contains(" a.txt:2:matchline"), "{body}");
        assert!(body.contains("+a.txt:3:after"), "{body}");
    }

    #[tokio::test]
    async fn grep_empty_pattern_errors() {
        let tempdir = tempdir().expect("tempdir");
        let err = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "" }),
        )
        .await
        .expect_err("empty pattern should error");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("empty")));
    }

    #[tokio::test]
    async fn grep_no_matches_returns_zero_count() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(tempdir.path(), &[("a.txt", "nothing here\n")]);
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "xyz" }),
        )
        .await
        .expect("grep");
        assert_eq!(result.details["match_count"], 0);
        assert!(text_body(&result).contains("No matches"));
    }

    #[tokio::test]
    async fn grep_long_lines_truncated_with_ellipsis() {
        let tempdir = tempdir().expect("tempdir");
        let long = "a".repeat(1_000);
        let content = format!("{long}match\n");
        make_tree(tempdir.path(), &[("a.txt", content.as_str())]);
        let result = run_grep(
            tempdir.path(),
            serde_json::json!({ "pattern": "match" }),
        )
        .await
        .expect("grep");
        let body = text_body(&result);
        assert!(body.contains('…'), "long line should be truncated: {body}");
    }
}
