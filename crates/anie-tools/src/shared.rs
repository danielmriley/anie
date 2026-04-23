use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anie_agent::ToolError;
use anie_protocol::{ContentBlock, ToolResult};

// --- Plan 07 PR-A: shared truncation helpers used by grep,
// bash, and any future tool that emits line- or byte-capped
// output. One policy per cap type instead of slightly-
// different implementations in every module.

/// Truncate a line to at most `max_chars` characters, appending
/// `…` when truncation happens. Zero-copy when the line already
/// fits — the caller's `&str` is borrowed through `Cow`
/// unchanged. Returns `(content, truncated)`.
pub(crate) fn truncate_line_to_chars(line: &str, max_chars: usize) -> (Cow<'_, str>, bool) {
    if line.chars().count() <= max_chars {
        return (Cow::Borrowed(line), false);
    }
    let mut truncated: String = line.chars().take(max_chars).collect();
    truncated.push('…');
    (Cow::Owned(truncated), true)
}

/// Check whether appending `addition_len` bytes to a buffer at
/// `current_len` would overflow a `byte_limit`. Saturating-add
/// protected.
#[must_use]
pub(crate) fn would_exceed_byte_limit(
    current_len: usize,
    addition_len: usize,
    byte_limit: usize,
) -> bool {
    current_len.saturating_add(addition_len) > byte_limit
}

pub(crate) const MAX_READ_LINES: usize = 2_000;
pub(crate) const MAX_READ_BYTES: usize = 50 * 1024;
pub(crate) const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
pub(crate) const IO_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub(crate) fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let requested = Path::new(path);
    if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        cwd.join(requested)
    }
}

pub(crate) fn text_result(text: String, details: serde_json::Value) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text { text }],
        details,
    }
}

pub(crate) fn required_string_arg<'a>(
    args: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::ExecutionFailed(format!("Missing '{key}' argument")))
}

pub(crate) fn parse_optional_usize_arg(
    args: &serde_json::Value,
    key: &str,
) -> Result<Option<usize>, ToolError> {
    match args.get(key) {
        None => Ok(None),
        Some(value) => {
            let number = value.as_u64().ok_or_else(|| {
                ToolError::ExecutionFailed(format!("'{key}' must be a non-negative integer"))
            })?;
            usize::try_from(number).map(Some).map_err(|_| {
                ToolError::ExecutionFailed(format!("'{key}' is too large for this platform"))
            })
        }
    }
}

pub(crate) fn parse_optional_timeout_secs(
    args: &serde_json::Value,
) -> Result<Option<DurationSpec>, ToolError> {
    let Some(timeout_value) = args.get("timeout") else {
        return Ok(None);
    };

    let seconds = timeout_value.as_f64().ok_or_else(|| {
        ToolError::ExecutionFailed("'timeout' must be a number of seconds".into())
    })?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(ToolError::ExecutionFailed(
            "'timeout' must be a positive number of seconds".into(),
        ));
    }

    Ok(Some(DurationSpec::from_seconds(seconds)))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DurationSpec {
    pub seconds: f64,
}

impl DurationSpec {
    fn from_seconds(seconds: f64) -> Self {
        Self { seconds }
    }

    pub fn as_duration(self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.seconds)
    }

    pub fn whole_seconds(self) -> u64 {
        self.seconds.ceil() as u64
    }
}

pub(crate) fn trim_to_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &value[..boundary]
}
