use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use similar::{ChangeTag, TextDiff};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::{
    FileMutationQueue,
    shared::{required_string_arg, resolve_path, text_result},
};

pub(crate) const MAX_EDIT_COUNT: usize = 100;
pub(crate) const MAX_EDIT_OLD_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_EDIT_NEW_TEXT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_EDIT_ARGUMENT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_EDIT_INPUT_FILE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const MAX_EDIT_OUTPUT_FILE_BYTES: usize =
    MAX_EDIT_INPUT_FILE_BYTES + (MAX_EDIT_ARGUMENT_BYTES / 2);

/// Apply one or more exact text replacements to a file.
pub struct EditTool {
    cwd: Arc<PathBuf>,
    mutation_queue: Arc<FileMutationQueue>,
}

impl EditTool {
    /// Create an edit tool with its own file-mutation queue. Relative
    /// paths resolve from `cwd`; absolute paths are allowed.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self::with_queue(cwd, Arc::new(FileMutationQueue::new()))
    }

    /// Create an edit tool using a shared file-mutation queue.
    /// Relative paths resolve from `cwd`; absolute paths are allowed.
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
impl Tool for EditTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit".into(),
            description: "Edit a single file using exact text replacement. Relative paths resolve from the session cwd; absolute paths are allowed. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit. Relative paths resolve from the session cwd; absolute paths are allowed." },
                    "edits": {
                        "type": "array",
                        "maxItems": MAX_EDIT_COUNT,
                        "description": "One or more targeted replacements. Each edit is matched against the original file, not after earlier edits are applied.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string", "maxLength": MAX_EDIT_OLD_TEXT_BYTES, "description": "Exact text for one targeted replacement" },
                                "newText": { "type": "string", "maxLength": MAX_EDIT_NEW_TEXT_BYTES, "description": "Replacement text for this targeted edit" }
                            },
                            "required": ["oldText", "newText"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["path", "edits"],
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
        let edits = parse_edits(&args)?;
        let abs_path = self.resolve_path(path);
        let mutation_queue = Arc::clone(&self.mutation_queue);

        mutation_queue
            .with_lock(&abs_path, || async {
                if cancel.is_cancelled() {
                    return Err(ToolError::Aborted);
                }

                let bytes = tokio::fs::read(&abs_path).await.map_err(|error| {
                    ToolError::ExecutionFailed(format!("Failed to read {path}: {error}"))
                })?;
                if bytes.len() > MAX_EDIT_INPUT_FILE_BYTES {
                    return Err(ToolError::ExecutionFailed(format!(
                        "{path} is {} bytes; edit input files are limited to {MAX_EDIT_INPUT_FILE_BYTES} bytes. Split the file or use a smaller target.",
                        bytes.len(),
                    )));
                }
                let (has_bom, text) = decode_utf8_with_bom(&bytes).map_err(|error| {
                    ToolError::ExecutionFailed(format!(
                        "Failed to decode {path} as UTF-8 text: {error}"
                    ))
                })?;
                let line_ending = detect_line_ending(&text);
                let normalized = normalize_to_lf(&text);
                let (new_normalized, diff) = apply_edits(&normalized, &edits, path)?;
                let restored = restore_line_endings(&new_normalized, line_ending);
                let output_bytes = encode_utf8_with_bom(&restored, has_bom);
                if output_bytes.len() > MAX_EDIT_OUTPUT_FILE_BYTES {
                    return Err(ToolError::ExecutionFailed(format!(
                        "edited {path} would be {} bytes; edit outputs are limited to {MAX_EDIT_OUTPUT_FILE_BYTES} bytes. Split this into smaller edit calls.",
                        output_bytes.len(),
                    )));
                }

                tokio::fs::write(&abs_path, output_bytes)
                    .await
                    .map_err(|error| {
                        ToolError::ExecutionFailed(format!("Failed to write {path}: {error}"))
                    })?;

                Ok(text_result(
                    format!(
                        "Applied {} edit{} to {}",
                        edits.len(),
                        if edits.len() == 1 { "" } else { "s" },
                        path,
                    ),
                    serde_json::json!({
                        "path": path,
                        "edits": edits.len(),
                        "diff": diff,
                    }),
                ))
            })
            .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Edit {
    old_text: String,
    new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchedEdit {
    edit_index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

fn parse_edits(args: &serde_json::Value) -> Result<Vec<Edit>, ToolError> {
    let edits = args
        .get("edits")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ToolError::ExecutionFailed("Missing 'edits' argument".into()))?;
    if edits.is_empty() {
        return Err(ToolError::ExecutionFailed(
            "'edits' must contain at least one replacement".into(),
        ));
    }
    if edits.len() > MAX_EDIT_COUNT {
        return Err(ToolError::ExecutionFailed(format!(
            "'edits' contains {} replacements; at most {MAX_EDIT_COUNT} are allowed per edit call. Split this into smaller edit calls.",
            edits.len(),
        )));
    }

    let mut total_edit_bytes = 0usize;
    edits
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let old_text = value
                .get("oldText")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed(format!("edit #{index} is missing 'oldText'"))
                })?;
            let new_text = value
                .get("newText")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed(format!("edit #{index} is missing 'newText'"))
                })?;
            enforce_edit_text_limit(index, "oldText", old_text, MAX_EDIT_OLD_TEXT_BYTES)?;
            enforce_edit_text_limit(index, "newText", new_text, MAX_EDIT_NEW_TEXT_BYTES)?;
            total_edit_bytes = total_edit_bytes
                .saturating_add(old_text.len())
                .saturating_add(new_text.len());
            if total_edit_bytes > MAX_EDIT_ARGUMENT_BYTES {
                return Err(ToolError::ExecutionFailed(format!(
                    "edit arguments are {total_edit_bytes} bytes; at most {MAX_EDIT_ARGUMENT_BYTES} bytes are allowed per edit call. Split this into smaller edit calls.",
                )));
            }
            Ok(Edit {
                old_text: normalize_to_lf(old_text),
                new_text: normalize_to_lf(new_text),
            })
        })
        .collect()
}

fn enforce_edit_text_limit(
    index: usize,
    field: &str,
    value: &str,
    limit: usize,
) -> Result<(), ToolError> {
    if value.len() > limit {
        return Err(ToolError::ExecutionFailed(format!(
            "edit #{index} {field} is {} bytes; at most {limit} bytes are allowed. Split this into smaller edit calls.",
            value.len(),
        )));
    }
    Ok(())
}

fn apply_edits(content: &str, edits: &[Edit], path: &str) -> Result<(String, String), ToolError> {
    let mut matched = Vec::with_capacity(edits.len());
    // Plan 07 PR-D: lazily compute the normalized content +
    // index map at most once per edit batch. Most batches
    // hit the exact-match fast path on every edit and never
    // need fuzzy normalization; the first fuzzy fallback
    // materializes the cache, and subsequent fuzzy edits in
    // the same batch reuse it.
    let mut fuzzy_cache: Option<(String, Vec<usize>)> = None;

    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "edit #{index} for {path} has an empty oldText",
            )));
        }

        let exact_matches = find_all_occurrences(content, &edit.old_text);
        if exact_matches.len() > 1 {
            return Err(ToolError::ExecutionFailed(format!(
                "edit #{index} for {path} matched {} regions; make oldText unique",
                exact_matches.len(),
            )));
        }
        if let Some((start, end)) = exact_matches.first().copied() {
            matched.push(MatchedEdit {
                edit_index: index,
                start,
                end,
                new_text: edit.new_text.clone(),
            });
            continue;
        }

        let fuzzy_cache = fuzzy_cache.get_or_insert_with(|| normalize_for_fuzzy_match(content));
        let fuzzy_matches = fuzzy_find_all_occurrences_in_normalized(
            &fuzzy_cache.0,
            &fuzzy_cache.1,
            content.len(),
            &edit.old_text,
        );
        if fuzzy_matches.is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "edit #{index} for {path} did not match anything",
            )));
        }
        if fuzzy_matches.len() > 1 {
            return Err(ToolError::ExecutionFailed(format!(
                "edit #{index} for {path} matched {} fuzzy regions; make oldText more specific",
                fuzzy_matches.len(),
            )));
        }
        let (start, end) = fuzzy_matches[0];
        matched.push(MatchedEdit {
            edit_index: index,
            start,
            end,
            new_text: edit.new_text.clone(),
        });
    }

    matched.sort_by_key(|edit| edit.start);
    for pair in matched.windows(2) {
        let left = &pair[0];
        let right = &pair[1];
        if left.end > right.start {
            return Err(ToolError::ExecutionFailed(format!(
                "edit #{} overlaps edit #{} in {path}; merge them into one replacement",
                left.edit_index, right.edit_index,
            )));
        }
    }

    let mut updated = content.to_string();
    for edit in matched.iter().rev() {
        updated.replace_range(edit.start..edit.end, &edit.new_text);
    }
    let diff = render_diff(content, &updated);
    Ok((updated, diff))
}

fn find_all_occurrences(content: &str, needle: &str) -> Vec<(usize, usize)> {
    content
        .match_indices(needle)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect()
}

fn fuzzy_find_all_occurrences_in_normalized(
    normalized_content: &str,
    index_map: &[usize],
    original_content_len: usize,
    needle: &str,
) -> Vec<(usize, usize)> {
    let normalized_needle = normalize_fuzzy_pattern(needle);
    if normalized_needle.is_empty() {
        return Vec::new();
    }

    normalized_content
        .match_indices(&normalized_needle)
        .map(|(start_byte, matched)| {
            let end_byte = start_byte + matched.len();
            let start_char = normalized_content[..start_byte].chars().count();
            let end_char = normalized_content[..end_byte].chars().count();
            let start = index_map[start_char];
            let end = if end_char < index_map.len() {
                index_map[end_char]
            } else {
                original_content_len
            };
            (start, end)
        })
        .collect()
}

fn normalize_to_lf(value: &str) -> String {
    // Plan 07 PR-E: single-pass CRLF + CR → LF normalization.
    // The previous shape was `replace("\r\n", "\n").replace('\r', "\n")`
    // which allocates a new String on each `replace` call — two
    // full passes over the input for a file that's already LF-
    // normalized. Now: one allocation sized to the source, one
    // pass.
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            // Collapse "\r\n" into "\n"; bare "\r" also becomes "\n".
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

fn detect_line_ending(value: &str) -> LineEnding {
    if value.contains("\r\n") {
        LineEnding::CrLf
    } else if value.contains('\r') {
        LineEnding::Cr
    } else {
        LineEnding::Lf
    }
}

fn restore_line_endings(value: &str, line_ending: LineEnding) -> String {
    match line_ending {
        LineEnding::Lf => value.to_string(),
        LineEnding::CrLf => value.replace('\n', "\r\n"),
        LineEnding::Cr => value.replace('\n', "\r"),
    }
}

fn decode_utf8_with_bom(bytes: &[u8]) -> Result<(bool, String), std::string::FromUtf8Error> {
    const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    let has_bom = bytes.starts_with(UTF8_BOM);
    let text = String::from_utf8(if has_bom {
        bytes[UTF8_BOM.len()..].to_vec()
    } else {
        bytes.to_vec()
    })?;
    Ok((has_bom, text))
}

fn encode_utf8_with_bom(value: &str, has_bom: bool) -> Vec<u8> {
    const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
    // Plan 07 PR-E: size the buffer up front so extend_from_slice
    // doesn't grow it (one reallocation avoided for files with
    // a BOM; the non-BOM path sizes exactly).
    let capacity = value.len() + if has_bom { UTF8_BOM.len() } else { 0 };
    let mut bytes = Vec::with_capacity(capacity);
    if has_bom {
        bytes.extend_from_slice(UTF8_BOM);
    }
    bytes.extend_from_slice(value.as_bytes());
    bytes
}

fn normalize_for_fuzzy_match(value: &str) -> (String, Vec<usize>) {
    let mut normalized = String::new();
    let mut index_map = Vec::new();
    let mut chars = value.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        if ch == '\n' {
            normalized.push('\n');
            index_map.push(index);
            continue;
        }

        if ch.is_whitespace() {
            normalized.push(' ');
            index_map.push(index);
            while let Some((_, next)) = chars.peek().copied() {
                if next != '\n' && next.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }
            continue;
        }

        normalized.push(ch);
        index_map.push(index);
    }

    (normalized, index_map)
}

fn normalize_fuzzy_pattern(value: &str) -> String {
    let (normalized, _) = normalize_for_fuzzy_match(value);
    normalized
}

fn render_diff(original: &str, updated: &str) -> String {
    let diff = TextDiff::from_lines(original, updated);
    let mut rendered = String::new();
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        rendered.push(prefix);
        rendered.push_str(change.value());
        if !change.value().ends_with('\n') {
            rendered.push('\n');
        }
    }
    rendered.trim_end().to_string()
}
