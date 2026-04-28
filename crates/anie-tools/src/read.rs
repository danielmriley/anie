use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError, ToolExecutionContext, effective_tool_output_budget};
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
    /// Create a read tool with the provided working directory as the
    /// base for relative paths. Absolute paths are allowed.
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
            description: "Read file contents. Relative paths resolve from the session cwd; absolute paths are allowed. Supports text files and images (jpg, png, gif, webp). Images are sent as attachments. For text files, output is truncated to 2000 lines or 50KB (whichever is hit first). When you need the full file, continue with offset until complete.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read. Relative paths resolve from the session cwd; absolute paths are allowed." },
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
        ctx: &ToolExecutionContext,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let path = required_string_arg(&args, "path")?;
        let offset = parse_optional_usize_arg(&args, "offset")?.unwrap_or(1);
        let limit = parse_optional_usize_arg(&args, "limit")?;
        let abs_path = self.resolve_path(path);
        // Plan 05 PR C: scale the byte cap with the model's
        // context window. `MAX_READ_BYTES` (50 KiB) remains the
        // upper bound; small-window models get a tighter cap
        // floored at `MIN_TOOL_OUTPUT_BUDGET_BYTES` so a single
        // file read can't blow the model's input.
        let byte_budget = usize::try_from(effective_tool_output_budget(
            ctx.context_window,
            MAX_READ_BYTES as u64,
        ))
        .unwrap_or(MAX_READ_BYTES);

        // Image branch: check size with metadata BEFORE reading
        // the body into memory. Without this, a 1 GiB image
        // would allocate 1 GiB just to fail the cap check on
        // the next line. PR 5.1 of `docs/code_review_2026-04-27/`.
        if is_image_path(&abs_path) {
            let meta = tokio::fs::metadata(&abs_path).await.map_err(|error| {
                ToolError::ExecutionFailed(format!("Failed to stat {path}: {error}"))
            })?;
            if meta.len() > MAX_IMAGE_BYTES {
                return Err(ToolError::ExecutionFailed(format!(
                    "Image {path} is too large to read ({} bytes > {} bytes)",
                    meta.len(),
                    MAX_IMAGE_BYTES
                )));
            }
            let bytes = tokio::fs::read(&abs_path).await.map_err(|error| {
                ToolError::ExecutionFailed(format!("Failed to read {path}: {error}"))
            })?;
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

        // Streaming text read: open the file and walk it
        // line-by-line, stopping the moment we have enough to
        // satisfy `limit` / `MAX_READ_LINES` / `MAX_READ_BYTES`.
        // PR 5.2 of `docs/code_review_2026-04-27/`. Memory
        // usage scales with the output cap rather than file
        // size — a 10 GiB log file with `limit = 20` returns
        // bounded output without ever allocating the file
        // body.
        let file = tokio::fs::File::open(&abs_path).await.map_err(|error| {
            ToolError::ExecutionFailed(format!("Failed to read {path}: {error}"))
        })?;
        let mut reader = tokio::io::BufReader::new(file);

        let start_line = offset.saturating_sub(1);
        let user_limit = limit;
        let mut shown_lines: Vec<String> = Vec::new();
        let mut bytes_used: usize = 0;
        let mut current_line_idx: usize = 0;
        let mut total_bytes_seen: u64 = 0;
        let mut truncated = false;
        let mut line_buf: Vec<u8> = Vec::new();

        loop {
            // Stop if the user-supplied limit is satisfied:
            // they got exactly what they asked for, no
            // truncation flag.
            if let Some(lim) = user_limit
                && shown_lines.len() >= lim
            {
                break;
            }

            line_buf.clear();
            let (n, ending) = read_one_line(&mut reader, &mut line_buf, MAX_LINE_BUFFER_BYTES)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("Failed to read {path}: {e}")))?;
            if n == 0 {
                break;
            }
            total_bytes_seen += n as u64;

            // Binary detection: any chunk containing a NUL
            // byte fails open as "binary, can't display." We
            // stream chunk-by-chunk, so this only catches NUL
            // bytes within the bytes we've actually read —
            // good enough for the current heuristic.
            if line_buf.contains(&0) {
                return Ok(text_result(
                    format!("{path} appears to be a binary file and cannot be displayed as text."),
                    serde_json::json!({
                        "path": path,
                        "binary": true,
                        "bytes": total_bytes_seen,
                    }),
                ));
            }

            // Skip lines until we reach `offset` (1-indexed).
            if current_line_idx < start_line {
                current_line_idx += 1;
                continue;
            }
            current_line_idx += 1;

            // Strip line ending (LF or CRLF) before measuring.
            // Only strip when `ending == Newline`; if we hit
            // the line-buffer cap or EOF mid-line, the bytes
            // we have are not delimited by `\n` and shouldn't
            // lose a trailing byte that just happens to look
            // like one.
            let mut bytes = line_buf.as_slice();
            if matches!(ending, LineEnd::Newline) {
                if bytes.ends_with(b"\n") {
                    bytes = &bytes[..bytes.len() - 1];
                }
                if bytes.ends_with(b"\r") {
                    bytes = &bytes[..bytes.len() - 1];
                }
            }
            // Per-line UTF-8-lossy decode is safe: the line
            // delimiter `\n` (0x0A) is never part of a
            // multi-byte UTF-8 sequence, so a multi-byte char
            // never spans line boundaries in valid UTF-8 text.
            let line = String::from_utf8_lossy(bytes).into_owned();

            // Hard line cap.
            if shown_lines.len() >= MAX_READ_LINES {
                truncated = true;
                break;
            }

            // Byte cap (scaled by `effective_tool_output_budget`).
            let separator_len = usize::from(!shown_lines.is_empty());
            let candidate_len = separator_len + line.len();
            if bytes_used + candidate_len > byte_budget {
                // Single line wider than the entire byte cap
                // — keep a partial prefix on a UTF-8 boundary
                // so the agent at least sees the head of the
                // line.
                if shown_lines.is_empty() {
                    shown_lines.push(trim_to_char_boundary(&line, byte_budget).to_string());
                }
                truncated = true;
                break;
            }
            bytes_used += candidate_len;
            shown_lines.push(line);

            // The line buffer hit its cap before finding a
            // newline. We've taken what we can; reading more
            // bytes of this same logical line would only
            // inflate memory. Stop and let the caller re-fetch
            // with a higher offset if needed.
            if matches!(ending, LineEnd::Cap) {
                truncated = true;
                break;
            }
        }

        let mut text = shown_lines.join("\n");
        // Footer wording change vs. the pre-streaming
        // implementation: precise "remaining N lines not
        // shown" required scanning the entire file to count
        // total lines. Streaming reads only as much as needed,
        // so we trade exact remaining-line math for bounded
        // memory and surface a less precise message instead.
        // PR 5.2 / 5.3 of `docs/code_review_2026-04-27/`.
        if truncated {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("[output truncated. Use offset to read more.]");
        }

        let text_len = text.len();
        let shown_count = shown_lines.len();
        Ok(text_result(
            text,
            serde_json::json!({
                "path": path,
                "lines": shown_count,
                "bytes": text_len,
                "truncated": truncated,
                "offset": offset,
            }),
        ))
    }
}

fn is_image_path(path: &Path) -> bool {
    // Plan 07 PR-G: case-insensitive extension match without
    // allocating a lowercased copy. `eq_ignore_ascii_case`
    // compares byte-by-byte folded; zero allocation.
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg", "gif", "webp"]
                .iter()
                .any(|expected| extension.eq_ignore_ascii_case(expected))
        })
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

/// Hard cap on a single line's buffer during streaming reads.
/// `read_one_line` stops accumulating beyond this even if no
/// newline has been found, so a pathological newline-less
/// file can't grow `line_buf` to the file size. Set to 4×
/// `MAX_READ_BYTES` so a single very long line still has
/// enough headroom to be trimmed on a UTF-8 boundary against
/// the displayed cap with reasonable margin. PR 5.2 of
/// `docs/code_review_2026-04-27/`.
const MAX_LINE_BUFFER_BYTES: usize = 4 * MAX_READ_BYTES;

/// How `read_one_line` finished its read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnd {
    /// A `\n` byte was found and the line ends with it.
    Newline,
    /// EOF was reached without finding `\n` (the file's last
    /// line had no trailing newline).
    Eof,
    /// `hard_cap` bytes were buffered before either a newline
    /// or EOF was reached. The caller has the first `hard_cap`
    /// bytes of a line and should treat the line as truncated.
    Cap,
}

/// Read one line from `reader` into `buf`, capped at
/// `hard_cap` bytes regardless of where the next newline is.
/// Returns `(bytes_read, how_it_ended)`. The cap exists so a
/// pathological newline-less file (or a malicious one) cannot
/// grow `buf` to the file size — `read_until(b'\n', ...)`
/// reads to EOF in that case.
async fn read_one_line<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    hard_cap: usize,
) -> std::io::Result<(usize, LineEnd)>
where
    R: AsyncBufRead + Unpin,
{
    use std::pin::Pin;

    let mut total = 0usize;
    loop {
        if total >= hard_cap {
            return Ok((total, LineEnd::Cap));
        }
        // Borrow ends inside this scope so we can call
        // `consume` (which mutably re-borrows the reader)
        // afterwards.
        let (take, found_newline) = {
            let available = AsyncBufReadExt::fill_buf(reader).await?;
            if available.is_empty() {
                return Ok((total, LineEnd::Eof));
            }
            let remaining = hard_cap - total;
            let look_len = available.len().min(remaining);
            let look = &available[..look_len];
            if let Some(pos) = look.iter().position(|&b| b == b'\n') {
                let take = pos + 1;
                buf.extend_from_slice(&available[..take]);
                (take, true)
            } else {
                buf.extend_from_slice(look);
                (look_len, false)
            }
        };
        Pin::new(&mut *reader).consume(take);
        total += take;
        if found_newline {
            return Ok((total, LineEnd::Newline));
        }
    }
}
