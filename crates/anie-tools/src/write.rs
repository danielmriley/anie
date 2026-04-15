use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::{
    FileMutationQueue,
    shared::{required_string_arg, resolve_path, text_result},
};

/// Write text content to a file, creating parent directories as needed.
pub struct WriteTool {
    cwd: Arc<PathBuf>,
    mutation_queue: Arc<FileMutationQueue>,
}

impl WriteTool {
    /// Create a write tool with its own file-mutation queue.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self::with_queue(cwd, Arc::new(FileMutationQueue::new()))
    }

    /// Create a write tool using a shared file-mutation queue.
    #[must_use]
    pub fn with_queue<P: Into<PathBuf>>(cwd: P, mutation_queue: Arc<FileMutationQueue>) -> Self {
        Self {
            cwd: Arc::new(cwd.into()),
            mutation_queue,
        }
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        resolve_path(self.cwd.as_ref(), path)
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write".into(),
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write" },
                    "content": { "type": "string", "description": "Content to write to the file" }
                },
                "required": ["path", "content"],
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
        let path = required_string_arg(&args, "path")?;
        let content = required_string_arg(&args, "content")?;
        let abs_path = self.resolve_path(path);
        let mutation_queue = Arc::clone(&self.mutation_queue);

        mutation_queue
            .with_lock(&abs_path, || async {
                if cancel.is_cancelled() {
                    return Err(ToolError::Aborted);
                }

                if let Some(parent) = abs_path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|error| {
                        ToolError::ExecutionFailed(format!(
                            "Failed to create parent directories for {path}: {error}"
                        ))
                    })?;
                }

                tokio::fs::write(&abs_path, content)
                    .await
                    .map_err(|error| {
                        ToolError::ExecutionFailed(format!("Failed to write {path}: {error}"))
                    })?;

                let lines = content.lines().count();
                let bytes = content.len();
                Ok(text_result(
                    format!("Successfully wrote {path} ({lines} lines, {bytes} bytes)"),
                    serde_json::json!({
                        "path": path,
                        "lines": lines,
                        "bytes": bytes,
                    }),
                ))
            })
            .await
    }
}
