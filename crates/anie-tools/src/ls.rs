//! Directory-listing tool.
//!
//! One entry per line, with `/` suffix on directories and `*`
//! suffix on executables. Relative paths resolve from the session cwd;
//! absolute paths are allowed intentionally because tools are not
//! sandboxed.
//! Default limit of 500 entries matches pi
//! (`packages/coding-agent/src/core/tools/ls.ts`).

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::shared::{resolve_path, text_result};

const DEFAULT_LS_LIMIT: usize = 500;

/// List the contents of a directory.
pub struct LsTool {
    cwd: Arc<PathBuf>,
}

impl LsTool {
    /// Create an ls tool with the provided working directory as the
    /// default path and base for relative paths.
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
impl Tool for LsTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "ls".into(),
            description: "List the contents of a directory. Relative paths resolve from the session cwd; absolute paths are allowed. Directories end with `/`, executables end with `*`. Hidden files are omitted unless `show_hidden: true`. Limit defaults to 500 entries.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory to list. Defaults to the session cwd. Relative paths resolve from cwd; absolute paths are allowed."
                    },
                    "show_hidden": {
                        "type": "boolean",
                        "description": "Include entries that start with `.` (default false)."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of entries to return (default 500)."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<anie_protocol::ToolResult>>,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let show_hidden = args
            .get("show_hidden")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(DEFAULT_LS_LIMIT))
            .unwrap_or(DEFAULT_LS_LIMIT);
        let path = self.resolve_path(args.get("path").and_then(serde_json::Value::as_str));

        let metadata = tokio::fs::metadata(&path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!("cannot stat {}: {error}", path.display()))
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::ExecutionFailed(format!(
                "{} is not a directory",
                path.display()
            )));
        }

        let mut read_dir = tokio::fs::read_dir(&path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!("cannot read {}: {error}", path.display()))
        })?;

        let mut entries: Vec<String> = Vec::new();
        let mut truncated = false;
        while let Some(entry) = read_dir
            .next_entry()
            .await
            .map_err(|error| ToolError::ExecutionFailed(format!("read_dir: {error}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            if entries.len() >= limit {
                truncated = true;
                break;
            }

            let entry_metadata = entry
                .metadata()
                .await
                .map_err(|error| ToolError::ExecutionFailed(format!("metadata: {error}")))?;
            let suffix = if entry_metadata.is_dir() {
                "/"
            } else if is_executable(&entry_metadata) {
                "*"
            } else {
                ""
            };
            entries.push(format!("{name}{suffix}"));
        }
        entries.sort();

        let count = entries.len();
        let mut output = entries.join("\n");
        if !output.is_empty() {
            output.push('\n');
        }
        if truncated {
            output.push_str(&format!(
                "[Truncated: {limit} entries limit reached. Use a larger `limit`.]\n",
            ));
        }

        let details = serde_json::json!({
            "path": path.to_string_lossy(),
            "count": count,
            "truncated": truncated,
        });
        let body = if count == 0 {
            "Directory is empty.".to_string()
        } else {
            output
        };
        Ok(text_result(body, details))
    }
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use tempfile::tempdir;

    async fn run_ls(
        cwd: &Path,
        args: serde_json::Value,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let tool = LsTool::new(cwd);
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
    async fn ls_lists_files_and_subdirs_with_suffixes() {
        let tempdir = tempdir().expect("tempdir");
        std::fs::write(tempdir.path().join("file.txt"), "").expect("file");
        std::fs::create_dir(tempdir.path().join("subdir")).expect("subdir");
        let result = run_ls(tempdir.path(), serde_json::json!({}))
            .await
            .expect("ls");
        let body = text_body(&result);
        assert!(body.contains("file.txt"), "{body}");
        assert!(body.contains("subdir/"), "{body}");
        assert_eq!(result.details["count"], 2);
    }

    #[tokio::test]
    async fn ls_hides_dotfiles_by_default() {
        let tempdir = tempdir().expect("tempdir");
        std::fs::write(tempdir.path().join("visible.txt"), "").expect("file");
        std::fs::write(tempdir.path().join(".hidden"), "").expect("hidden");
        let result = run_ls(tempdir.path(), serde_json::json!({}))
            .await
            .expect("ls");
        let body = text_body(&result);
        assert!(body.contains("visible.txt"));
        assert!(!body.contains(".hidden"));
    }

    #[tokio::test]
    async fn ls_show_hidden_true_includes_dotfiles() {
        let tempdir = tempdir().expect("tempdir");
        std::fs::write(tempdir.path().join(".hidden"), "").expect("hidden");
        let result = run_ls(tempdir.path(), serde_json::json!({ "show_hidden": true }))
            .await
            .expect("ls");
        assert!(text_body(&result).contains(".hidden"));
    }

    #[tokio::test]
    async fn ls_nonexistent_path_errors_cleanly() {
        let tempdir = tempdir().expect("tempdir");
        let err = run_ls(
            tempdir.path(),
            serde_json::json!({ "path": "does-not-exist" }),
        )
        .await
        .expect_err("should error");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("cannot stat")));
    }

    #[tokio::test]
    async fn ls_non_directory_errors() {
        let tempdir = tempdir().expect("tempdir");
        std::fs::write(tempdir.path().join("file.txt"), "").expect("file");
        let err = run_ls(tempdir.path(), serde_json::json!({ "path": "file.txt" }))
            .await
            .expect_err("should error");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("not a directory")));
    }

    #[tokio::test]
    async fn ls_limit_truncates() {
        let tempdir = tempdir().expect("tempdir");
        for i in 0..20 {
            std::fs::write(tempdir.path().join(format!("f{i:02}.txt")), "").expect("write");
        }
        let result = run_ls(tempdir.path(), serde_json::json!({ "limit": 5 }))
            .await
            .expect("ls");
        assert_eq!(result.details["count"], 5);
        assert_eq!(result.details["truncated"], true);
        assert!(text_body(&result).contains("5 entries limit reached"));
    }

    #[tokio::test]
    async fn ls_empty_directory_returns_friendly_message() {
        let tempdir = tempdir().expect("tempdir");
        let sub = tempdir.path().join("empty");
        std::fs::create_dir(&sub).expect("mkdir");
        let result = run_ls(tempdir.path(), serde_json::json!({ "path": "empty" }))
            .await
            .expect("ls");
        assert!(text_body(&result).contains("empty"));
        assert_eq!(result.details["count"], 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn ls_marks_executables_with_star() {
        use std::os::unix::fs::PermissionsExt;
        let tempdir = tempdir().expect("tempdir");
        let bin = tempdir.path().join("runme.sh");
        std::fs::write(&bin, "#!/bin/sh\n").expect("write");
        let mut perms = std::fs::metadata(&bin).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod");
        let result = run_ls(tempdir.path(), serde_json::json!({}))
            .await
            .expect("ls");
        assert!(text_body(&result).contains("runme.sh*"));
    }
}
