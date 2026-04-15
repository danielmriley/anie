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

/// Apply one or more exact text replacements to a file.
pub struct EditTool {
    cwd: Arc<PathBuf>,
    mutation_queue: Arc<FileMutationQueue>,
}

impl EditTool {
    /// Create an edit tool with its own file-mutation queue.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self::with_queue(cwd, Arc::new(FileMutationQueue::new()))
    }

    /// Create an edit tool using a shared file-mutation queue.
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
            description: "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                    "edits": {
                        "type": "array",
                        "description": "One or more targeted replacements. Each edit is matched against the original file, not after earlier edits are applied.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string", "description": "Exact text for one targeted replacement" },
                                "newText": { "type": "string", "description": "Replacement text for this targeted edit" }
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
            Ok(Edit {
                old_text: normalize_to_lf(old_text),
                new_text: normalize_to_lf(new_text),
            })
        })
        .collect()
}

fn apply_edits(content: &str, edits: &[Edit], path: &str) -> Result<(String, String), ToolError> {
    let mut matched = Vec::with_capacity(edits.len());

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

        let fuzzy_matches = fuzzy_find_all_occurrences(content, &edit.old_text);
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

fn fuzzy_find_all_occurrences(content: &str, needle: &str) -> Vec<(usize, usize)> {
    let (normalized_content, index_map) = normalize_for_fuzzy_match(content);
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
                content.len()
            };
            (start, end)
        })
        .collect()
}

fn normalize_to_lf(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
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
    let mut bytes = Vec::new();
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
