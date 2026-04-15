use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::shared::{
    MAX_IMAGE_BYTES, MAX_READ_BYTES, MAX_READ_LINES, parse_optional_usize_arg, required_string_arg,
    resolve_path, text_result, trim_to_char_boundary,
};

/// Read a file from disk with truncation controls.
pub struct ReadTool {
    cwd: Arc<PathBuf>,
}

impl ReadTool {
    /// Create a read tool rooted at the provided working directory.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self {
            cwd: Arc::new(cwd.into()),
        }
    }

    fn resolve_path(&self, path: &str) -> PathBuf {
        resolve_path(self.cwd.as_ref(), path)
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read".into(),
            description: "Read file contents. Supports text files and images (jpg, png, gif, webp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). When you need the full file, continue with offset until complete.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                    "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                },
                "required": ["path"],
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
        let path = required_string_arg(&args, "path")?;
        let offset = parse_optional_usize_arg(&args, "offset")?.unwrap_or(1);
        let limit = parse_optional_usize_arg(&args, "limit")?;
        let abs_path = self.resolve_path(path);

        let bytes = tokio::fs::read(&abs_path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!("Failed to read {path}: {error}"))
        })?;

        if is_image_path(&abs_path) {
            let image_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if image_size > MAX_IMAGE_BYTES {
                return Err(ToolError::ExecutionFailed(format!(
                    "Image {path} is too large to read ({} bytes > {} bytes)",
                    bytes.len(),
                    MAX_IMAGE_BYTES
                )));
            }

            return Ok(anie_protocol::ToolResult {
                content: vec![anie_protocol::ContentBlock::Image {
                    media_type: image_media_type(&abs_path).to_string(),
                    data: STANDARD.encode(&bytes),
                }],
                details: serde_json::json!({
                    "path": path,
                    "media_type": image_media_type(&abs_path),
                    "bytes": bytes.len(),
                }),
            });
        }

        if bytes.contains(&0) {
            return Ok(text_result(
                format!("{path} appears to be a binary file and cannot be displayed as text."),
                serde_json::json!({
                    "path": path,
                    "binary": true,
                    "bytes": bytes.len(),
                }),
            ));
        }

        let raw_text = String::from_utf8_lossy(&bytes).into_owned();
        let lines: Vec<&str> = raw_text.lines().collect();
        let start_index = offset.saturating_sub(1).min(lines.len());
        let requested_end = limit
            .map(|count| start_index.saturating_add(count).min(lines.len()))
            .unwrap_or(lines.len());
        let requested_lines = &lines[start_index..requested_end];

        let mut shown_lines = Vec::new();
        let mut bytes_used = 0usize;
        let mut truncated = false;
        let mut partial_line_truncated = false;

        for (index, line) in requested_lines.iter().enumerate() {
            if index >= MAX_READ_LINES {
                truncated = true;
                break;
            }

            let candidate = if shown_lines.is_empty() {
                (*line).to_string()
            } else {
                format!("\n{line}")
            };
            if bytes_used + candidate.len() > MAX_READ_BYTES {
                if shown_lines.is_empty() {
                    shown_lines.push(trim_to_char_boundary(line, MAX_READ_BYTES).to_string());
                    partial_line_truncated = true;
                } else {
                    partial_line_truncated = true;
                }
                truncated = true;
                break;
            }
            bytes_used += candidate.len();
            shown_lines.push((*line).to_string());
        }

        if shown_lines.len() < requested_lines.len() {
            truncated = true;
        }

        let mut text = shown_lines.join("\n");
        if truncated {
            let remaining = requested_lines.len().saturating_sub(shown_lines.len())
                + usize::from(partial_line_truncated);
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "[remaining {remaining} lines not shown. Use offset to read more.]"
            ));
        }

        Ok(text_result(
            text.clone(),
            serde_json::json!({
                "path": path,
                "lines": shown_lines.len(),
                "bytes": text.len(),
                "truncated": truncated,
                "offset": offset,
            }),
        ))
    }
}

fn is_image_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()).map(|extension| extension.to_ascii_lowercase()),
        Some(extension) if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

fn image_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}
