//! Glob-based file finder.
//!
//! Wraps the `ignore` crate's walker with an override-style glob
//! filter — not Unix `find`. Parameter names and defaults match
//! pi's find tool (`packages/coding-agent/src/core/tools/find.ts`):
//! `limit: 1000` by default, `.gitignore`-aware, returns one
//! path per line.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::shared::{resolve_path, text_result};

const DEFAULT_FIND_LIMIT: usize = 1_000;
const BYTE_LIMIT: usize = 50 * 1024;

/// Find files matching a glob pattern, respecting `.gitignore`.
pub struct FindTool {
    cwd: Arc<PathBuf>,
}

impl FindTool {
    /// Create a find tool with the provided working directory as the
    /// default search root and base for relative paths.
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
impl Tool for FindTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "find".into(),
            description: "Find files matching a glob pattern (e.g. 'src/**/*.rs'). Relative paths resolve from the session cwd; absolute paths are allowed. Respects .gitignore by default. Returns one path per line, up to 1000 results or 50 KB of output.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (e.g. '*.rs', 'src/**/*.ts', '**/Cargo.toml')."
                    },
                    "path": {
                        "type": "string",
                        "description": "Search root directory. Defaults to the session cwd. Relative paths resolve from cwd; absolute paths are allowed."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of paths to return (default 1000)."
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
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(DEFAULT_FIND_LIMIT))
            .unwrap_or(DEFAULT_FIND_LIMIT);
        let search_root = self.resolve_path(args.get("path").and_then(serde_json::Value::as_str));
        let cwd = Arc::clone(&self.cwd);

        let cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag_for_watcher = Arc::clone(&cancel_flag);
        let cancel_watcher = cancel.clone();
        let watcher_handle = tokio::spawn(async move {
            cancel_watcher.cancelled().await;
            cancel_flag_for_watcher.store(true, Ordering::Relaxed);
        });

        let result = tokio::task::spawn_blocking(move || {
            run_find(&cwd, &search_root, &pattern, limit, &cancel_flag)
        })
        .await
        .map_err(|error| ToolError::ExecutionFailed(format!("find task join error: {error}")))??;
        watcher_handle.abort();

        let details = serde_json::json!({
            "pattern": result.pattern,
            "count": result.count,
            "truncated": result.truncated_reason.is_some(),
        });
        let body = if result.count == 0 {
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

struct FindResult {
    pattern: String,
    output: String,
    count: usize,
    truncated_reason: Option<String>,
}

fn run_find(
    cwd: &Path,
    search_root: &Path,
    pattern: &str,
    limit: usize,
    cancel: &AtomicBool,
) -> Result<FindResult, ToolError> {
    let mut override_builder = OverrideBuilder::new(search_root);
    override_builder
        .add(pattern)
        .map_err(|error| ToolError::ExecutionFailed(format!("invalid glob: {error}")))?;
    let overrides = override_builder
        .build()
        .map_err(|error| ToolError::ExecutionFailed(format!("glob build failed: {error}")))?;

    let mut walk_builder = WalkBuilder::new(search_root);
    walk_builder
        .standard_filters(true)
        .hidden(false)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .overrides(overrides);

    let mut output = String::new();
    let mut count = 0usize;
    let mut truncated_reason = None;

    for entry in walk_builder.build() {
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
        let line = format!("{display_path}\n");
        if output.len() + line.len() > BYTE_LIMIT {
            truncated_reason =
                Some("50 KB byte limit reached. Narrow the pattern or path.".to_string());
            break;
        }
        output.push_str(&line);
        count += 1;
        if count >= limit {
            truncated_reason = Some(format!(
                "{limit} result limit reached. Use a larger `limit` or narrow the pattern.",
            ));
            break;
        }
    }

    Ok(FindResult {
        pattern: pattern.to_string(),
        output,
        count,
        truncated_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_tree(dir: &Path, paths: &[&str]) {
        for p in paths {
            let full = dir.join(p);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(&full, "").expect("write");
        }
    }

    async fn run_find_tool(
        cwd: &Path,
        args: serde_json::Value,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let tool = FindTool::new(cwd);
        tool.execute("call", args, CancellationToken::new(), None)
            .await
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
    async fn find_returns_matching_paths() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(
            tempdir.path(),
            &["src/a.rs", "src/nested/b.rs", "docs/readme.md"],
        );
        let result = run_find_tool(tempdir.path(), serde_json::json!({ "pattern": "**/*.rs" }))
            .await
            .expect("find");
        let body = text_body(&result);
        assert!(body.contains("src/a.rs"), "{body}");
        assert!(body.contains("src/nested/b.rs"), "{body}");
        assert!(!body.contains("readme.md"), "{body}");
        assert_eq!(result.details["count"], 2);
    }

    #[tokio::test]
    async fn find_respects_gitignore() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(tempdir.path(), &[".gitignore", "target/out.rs", "src/a.rs"]);
        std::fs::write(tempdir.path().join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(tempdir.path().join(".git")).expect("git dir");
        let result = run_find_tool(tempdir.path(), serde_json::json!({ "pattern": "**/*.rs" }))
            .await
            .expect("find");
        let body = text_body(&result);
        assert!(body.contains("src/a.rs"));
        assert!(!body.contains("target/"));
    }

    #[tokio::test]
    async fn find_limit_truncates_with_footer() {
        let tempdir = tempdir().expect("tempdir");
        let mut paths = Vec::new();
        for i in 0..20 {
            paths.push(format!("f{i}.txt"));
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        make_tree(tempdir.path(), &refs);
        let result = run_find_tool(
            tempdir.path(),
            serde_json::json!({ "pattern": "*.txt", "limit": 5 }),
        )
        .await
        .expect("find");
        assert_eq!(result.details["count"], 5);
        assert_eq!(result.details["truncated"], true);
        assert!(text_body(&result).contains("5 result limit reached"));
    }

    #[tokio::test]
    async fn find_no_matches_returns_zero() {
        let tempdir = tempdir().expect("tempdir");
        make_tree(tempdir.path(), &["hello.txt"]);
        let result = run_find_tool(tempdir.path(), serde_json::json!({ "pattern": "*.rs" }))
            .await
            .expect("find");
        assert_eq!(result.details["count"], 0);
        assert!(text_body(&result).contains("No matches"));
    }

    #[tokio::test]
    async fn find_empty_pattern_errors() {
        let tempdir = tempdir().expect("tempdir");
        let err = run_find_tool(tempdir.path(), serde_json::json!({ "pattern": "" }))
            .await
            .expect_err("empty pattern should error");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("empty")));
    }

    #[tokio::test]
    async fn find_invalid_glob_errors_cleanly() {
        let tempdir = tempdir().expect("tempdir");
        let err = run_find_tool(tempdir.path(), serde_json::json!({ "pattern": "[invalid" }))
            .await
            .expect_err("invalid glob should error");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("glob")));
    }
}
