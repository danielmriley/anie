//! `apply_patch` tool: apply a Codex-style patch envelope carrying
//! multi-hunk, multi-file changes.
//!
//! The model emits the envelope; the tool parses it, matches each Update
//! hunk via the shared `text_match` engine, and applies every change
//! through `FileMutationQueue::with_locks` with validate-all-then-write-
//! all semantics — all changes in one call apply, or none do (no partial
//! application on a logical failure). See `docs/apply_patch_tool/README.md`.

use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError, ToolExecutionContext};
use anie_protocol::{ToolDef, ToolResult};

use crate::{
    FileMutationQueue,
    edit::{
        MAX_EDIT_INPUT_FILE_BYTES, MAX_EDIT_OUTPUT_FILE_BYTES, decode_utf8_with_bom,
        detect_line_ending, encode_utf8_with_bom, normalize_to_lf, restore_line_endings,
    },
    shared::{required_string_arg, resolve_path, text_result},
    text_match::{self, Edit, MatchKind},
};

/// One file-level operation parsed from a patch envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileOp {
    /// Create a new file with the given contents.
    Add { path: String, contents: String },
    /// Apply one or more replacement hunks to an existing file.
    Update { path: String, hunks: Vec<Edit> },
    /// Remove an existing file.
    Delete { path: String },
}

impl FileOp {
    fn path(&self) -> &str {
        match self {
            FileOp::Add { path, .. } | FileOp::Update { path, .. } | FileOp::Delete { path } => {
                path
            }
        }
    }
}

/// Parse a `*** Begin Patch` … `*** End Patch` envelope into file ops.
/// Strict: anything outside the envelope, an unrecognized `*** ` marker,
/// or a malformed body line is an error with a model-actionable message.
pub(crate) fn parse_patch(patch: &str) -> Result<Vec<FileOp>, ToolError> {
    let raw: Vec<&str> = patch.lines().collect();
    let mut i = 0;

    // Skip leading blank lines, then require the open marker.
    while i < raw.len() && raw[i].trim().is_empty() {
        i += 1;
    }
    if i >= raw.len() || raw[i].trim() != "*** Begin Patch" {
        return Err(err("patch must begin with `*** Begin Patch`"));
    }
    i += 1;

    let mut ops = Vec::new();
    loop {
        if i >= raw.len() {
            return Err(err("patch is missing its `*** End Patch` marker"));
        }
        let line = raw[i];
        if line.trim() == "*** End Patch" {
            i += 1;
            break;
        }

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            i += 1;
            let mut body = Vec::new();
            while i < raw.len() && !raw[i].starts_with("*** ") {
                let content = raw[i].strip_prefix('+').ok_or_else(|| {
                    err(format!(
                        "Add File `{}` body lines must each start with `+`; got: {}",
                        path.trim(),
                        raw[i]
                    ))
                })?;
                body.push(content);
                i += 1;
            }
            ops.push(FileOp::Add {
                path: path.trim().to_string(),
                contents: body.join("\n"),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            i += 1;
            ops.push(FileOp::Delete {
                path: path.trim().to_string(),
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            i += 1;
            let mut body = Vec::new();
            while i < raw.len() && !raw[i].starts_with("*** ") {
                body.push(raw[i]);
                i += 1;
            }
            let hunks = lower_update_hunks(path.trim(), &body)?;
            ops.push(FileOp::Update {
                path: path.trim().to_string(),
                hunks,
            });
        } else {
            return Err(err(format!(
                "unexpected line in patch (expected a `*** Add/Update/Delete File:` marker or `*** End Patch`): {line}"
            )));
        }
    }

    // Nothing but blanks may follow the end marker.
    while i < raw.len() {
        if !raw[i].trim().is_empty() {
            return Err(err(format!(
                "unexpected content after `*** End Patch`: {}",
                raw[i]
            )));
        }
        i += 1;
    }

    if ops.is_empty() {
        return Err(err("patch contains no file sections"));
    }
    Ok(ops)
}

/// Split an Update body into hunks (a `@@` line opens a new hunk and is
/// otherwise an ignored hint) and lower each to an `Edit`.
fn lower_update_hunks(path: &str, body: &[&str]) -> Result<Vec<Edit>, ToolError> {
    let mut hunks = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for &line in body {
        if line.starts_with("@@") {
            if !current.is_empty() {
                hunks.push(lower_one_hunk(path, &current)?);
                current.clear();
            }
            continue; // hint only
        }
        current.push(line);
    }
    if !current.is_empty() {
        hunks.push(lower_one_hunk(path, &current)?);
    }
    if hunks.is_empty() {
        return Err(err(format!("Update File `{path}` has no hunk lines")));
    }
    Ok(hunks)
}

/// Lower one hunk's body to an `{old_text, new_text}`: context + `-`
/// lines form the old block, context + `+` lines form the new block.
fn lower_one_hunk(path: &str, lines: &[&str]) -> Result<Edit, ToolError> {
    let mut old = Vec::new();
    let mut new = Vec::new();
    for &line in lines {
        if let Some(rest) = line.strip_prefix(' ') {
            old.push(rest);
            new.push(rest);
        } else if let Some(rest) = line.strip_prefix('-') {
            old.push(rest);
        } else if let Some(rest) = line.strip_prefix('+') {
            new.push(rest);
        } else if line.is_empty() {
            // A truly empty line is treated as blank context.
            old.push("");
            new.push("");
        } else {
            return Err(err(format!(
                "Update File `{path}` body lines must start with ' ', '-', or '+'; got: {line}"
            )));
        }
    }
    Ok(Edit {
        old_text: old.join("\n"),
        new_text: new.join("\n"),
    })
}

fn err(message: impl Into<String>) -> ToolError {
    ToolError::ExecutionFailed(message.into())
}

/// Apply a Codex-style patch envelope across one or more files.
pub struct ApplyPatchTool {
    cwd: Arc<PathBuf>,
    mutation_queue: Arc<FileMutationQueue>,
}

impl ApplyPatchTool {
    /// Build sharing an existing mutation queue, so `apply_patch`,
    /// `edit`, and `write` serialize against each other on shared paths.
    #[must_use]
    pub fn with_queue(cwd: impl Into<PathBuf>, mutation_queue: Arc<FileMutationQueue>) -> Self {
        Self {
            cwd: Arc::new(cwd.into()),
            mutation_queue,
        }
    }

    /// Build with a private queue (tests / standalone use).
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self::with_queue(cwd, Arc::new(FileMutationQueue::new()))
    }
}

/// A validated, ready-to-commit change for one file. Computed in the
/// validate phase; executed in the write phase only if every file
/// validated.
enum PlannedWrite {
    Write {
        abs: PathBuf,
        bytes: Vec<u8>,
        create_parents: bool,
    },
    Delete {
        abs: PathBuf,
    },
}

/// Per-file outcome surfaced in the result `details`.
struct FileReport {
    path: String,
    op: &'static str,
    diff: Option<String>,
    hunks: Option<usize>,
    fuzzy_hunks: Option<usize>,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "apply_patch".into(),
            description: "Apply a structured patch across one or more files in a single call. The patch is a `*** Begin Patch` / `*** End Patch` envelope with `*** Add File:`, `*** Update File:`, and `*** Delete File:` sections; Update hunks use ` ` context, `-` removal, and `+` addition lines. All changes apply together, or none if validation fails (no partial application). Prefer `edit` for one or two surgical replacements; use `apply_patch` for coordinated multi-hunk or multi-file changes. Whitespace-insensitive matching is used as a fallback when an exact match is not found.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "A *** Begin Patch / *** End Patch envelope of Add/Update/Delete File sections."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "When true, validate and return the combined diff without writing anything (default false)."
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let patch = required_string_arg(&args, "patch")?;
        let dry_run = args
            .get("dry_run")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let ops = parse_patch(patch)?;

        // Resolve every target path (same policy as edit/write) and lock
        // them all at once so the call is atomic against other mutations.
        let resolved: Vec<(FileOp, PathBuf)> = ops
            .into_iter()
            .map(|op| {
                let abs = resolve_path(self.cwd.as_ref(), op.path());
                (op, abs)
            })
            .collect();
        let lock_paths: Vec<PathBuf> = resolved.iter().map(|(_, abs)| abs.clone()).collect();

        self.mutation_queue
            .with_locks(&lock_paths, move || async move {
                apply_under_lock(resolved, &cancel, dry_run).await
            })
            .await
    }
}

/// Validate every file op against the on-disk state (no writes), then —
/// only if all validated, the call wasn't cancelled, and it isn't a dry
/// run — write them all.
async fn apply_under_lock(
    resolved: Vec<(FileOp, PathBuf)>,
    cancel: &CancellationToken,
    dry_run: bool,
) -> Result<ToolResult, ToolError> {
    // Reject a patch that targets the same file in more than one section.
    // Each Update is validated independently against the original on-disk
    // bytes, and the write phase applies sections sequentially to the same
    // path, so a second section would silently clobber the first — data
    // loss reported as success. Codex semantics are one operation per file.
    {
        let mut seen = std::collections::HashSet::new();
        for (op, abs) in &resolved {
            if !seen.insert(abs.as_path()) {
                return Err(err(format!(
                    "apply_patch: `{}` is targeted by more than one patch section; \
                     combine the changes into a single section (one operation per file)",
                    op.path()
                )));
            }
        }
    }

    let mut planned = Vec::new();
    let mut reports = Vec::new();

    for (op, abs) in &resolved {
        match op {
            FileOp::Add { path, contents } => {
                if abs.exists() {
                    return Err(err(format!(
                        "apply_patch: Add File `{path}` already exists; use Update File instead"
                    )));
                }
                // Source files conventionally end in a newline; the
                // parser strips the final one, so restore it.
                let body = if contents.is_empty() {
                    String::new()
                } else {
                    format!("{contents}\n")
                };
                let bytes = body.into_bytes();
                check_output_size(path, bytes.len())?;
                reports.push(FileReport {
                    path: path.clone(),
                    op: "add",
                    diff: Some(text_match::render_diff("", contents)),
                    hunks: None,
                    fuzzy_hunks: None,
                });
                planned.push(PlannedWrite::Write {
                    abs: abs.clone(),
                    bytes,
                    create_parents: true,
                });
            }
            FileOp::Delete { path } => {
                if !abs.exists() {
                    return Err(err(format!(
                        "apply_patch: Delete File `{path}` does not exist"
                    )));
                }
                reports.push(FileReport {
                    path: path.clone(),
                    op: "delete",
                    diff: None,
                    hunks: None,
                    fuzzy_hunks: None,
                });
                planned.push(PlannedWrite::Delete { abs: abs.clone() });
            }
            FileOp::Update { path, hunks } => {
                let bytes = tokio::fs::read(abs).await.map_err(|error| {
                    err(format!("apply_patch: could not read `{path}`: {error}"))
                })?;
                if bytes.len() > MAX_EDIT_INPUT_FILE_BYTES {
                    return Err(err(format!(
                        "apply_patch: `{path}` is {} bytes; update inputs are limited to {MAX_EDIT_INPUT_FILE_BYTES} bytes",
                        bytes.len()
                    )));
                }
                let (has_bom, text) = decode_utf8_with_bom(&bytes).map_err(|error| {
                    err(format!("apply_patch: `{path}` is not valid UTF-8: {error}"))
                })?;
                let line_ending = detect_line_ending(&text);
                let normalized = normalize_to_lf(&text);
                let outcome = text_match::apply_edits(&normalized, hunks, path)?;
                let restored = restore_line_endings(&outcome.updated, line_ending);
                let out_bytes = encode_utf8_with_bom(&restored, has_bom);
                check_output_size(path, out_bytes.len())?;
                let fuzzy = outcome
                    .kinds
                    .iter()
                    .filter(|kind| matches!(kind, MatchKind::Fuzzy))
                    .count();
                reports.push(FileReport {
                    path: path.clone(),
                    op: "update",
                    diff: Some(text_match::render_diff(&normalized, &outcome.updated)),
                    hunks: Some(hunks.len()),
                    fuzzy_hunks: Some(fuzzy),
                });
                planned.push(PlannedWrite::Write {
                    abs: abs.clone(),
                    bytes: out_bytes,
                    create_parents: false,
                });
            }
        }
    }

    // Dry run: everything validated, return the combined diff without
    // touching disk. This is the preview seam a future approval layer
    // would call into.
    if dry_run {
        return Ok(build_result(&reports, false));
    }

    if cancel.is_cancelled() {
        return Err(ToolError::Aborted);
    }

    // Write phase — every file validated. anie-specific (documented):
    // writes are direct (not temp-then-rename); validate-before-write is
    // the atomicity guarantee, and the residual multi-file crash window
    // is documented in the plan, not closed here. Uses `tokio::fs` (like
    // `edit`/`write`) so per-file I/O doesn't block the runtime worker.
    for write in planned {
        match write {
            PlannedWrite::Write {
                abs,
                bytes,
                create_parents,
            } => {
                if create_parents && let Some(parent) = abs.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|error| {
                        err(format!(
                            "apply_patch: could not create {}: {error}",
                            parent.display()
                        ))
                    })?;
                }
                tokio::fs::write(&abs, &bytes).await.map_err(|error| {
                    err(format!(
                        "apply_patch: failed to write {}: {error}",
                        abs.display()
                    ))
                })?;
            }
            PlannedWrite::Delete { abs } => {
                tokio::fs::remove_file(&abs).await.map_err(|error| {
                    err(format!(
                        "apply_patch: failed to delete {}: {error}",
                        abs.display()
                    ))
                })?;
            }
        }
    }

    Ok(build_result(&reports, true))
}

fn check_output_size(path: &str, len: usize) -> Result<(), ToolError> {
    if len > MAX_EDIT_OUTPUT_FILE_BYTES {
        return Err(err(format!(
            "apply_patch: `{path}` would be {len} bytes; outputs are limited to {MAX_EDIT_OUTPUT_FILE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn build_result(reports: &[FileReport], applied: bool) -> ToolResult {
    let summary = reports
        .iter()
        .map(|r| {
            let fuzzy = match r.fuzzy_hunks {
                Some(n) if n > 0 => format!(" ({n} matched ignoring whitespace)"),
                _ => String::new(),
            };
            format!("  {} {}{}", r.op, r.path, fuzzy)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let header = if applied {
        format!("Applied patch to {} file(s):", reports.len())
    } else {
        format!(
            "Validated patch for {} file(s) (dry run, nothing written):",
            reports.len()
        )
    };
    let files: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            let mut obj = serde_json::Map::new();
            obj.insert("path".into(), r.path.clone().into());
            obj.insert("op".into(), r.op.into());
            if let Some(diff) = &r.diff {
                obj.insert("diff".into(), diff.clone().into());
            }
            if let Some(hunks) = r.hunks {
                obj.insert("hunks".into(), hunks.into());
            }
            if let Some(fuzzy) = r.fuzzy_hunks {
                obj.insert("fuzzy_hunks".into(), fuzzy.into());
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    text_result(
        format!("{header}\n{summary}"),
        serde_json::json!({ "applied": applied, "files": files }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_patch_rejects_body_missing_begin_marker() {
        let e = parse_patch("*** Update File: a\n-x\n+y\n*** End Patch").expect_err("no begin");
        assert!(matches!(e, ToolError::ExecutionFailed(m) if m.contains("*** Begin Patch")));
    }

    #[test]
    fn parse_patch_rejects_unterminated_envelope_without_end_marker() {
        let e = parse_patch("*** Begin Patch\n*** Delete File: a").expect_err("no end");
        assert!(matches!(e, ToolError::ExecutionFailed(m) if m.contains("*** End Patch")));
    }

    #[test]
    fn parse_add_file_collects_plus_prefixed_body_as_contents() {
        let ops = parse_patch(
            "*** Begin Patch\n*** Add File: src/new.rs\n+fn main() {}\n+// done\n*** End Patch",
        )
        .expect("ok");
        assert_eq!(
            ops,
            vec![FileOp::Add {
                path: "src/new.rs".into(),
                contents: "fn main() {}\n// done".into(),
            }]
        );
    }

    #[test]
    fn parse_update_file_lowers_context_and_deletions_into_old_text() {
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.rs\n ctx\n-gone\n ctx2\n*** End Patch",
        )
        .expect("ok");
        match &ops[0] {
            FileOp::Update { hunks, .. } => assert_eq!(hunks[0].old_text, "ctx\ngone\nctx2"),
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_file_lowers_context_and_additions_into_new_text() {
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.rs\n ctx\n+added\n ctx2\n*** End Patch",
        )
        .expect("ok");
        match &ops[0] {
            FileOp::Update { hunks, .. } => assert_eq!(hunks[0].new_text, "ctx\nadded\nctx2"),
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[test]
    fn parse_update_file_treats_at_at_header_as_ignored_hint() {
        // Two @@ sections produce two hunks; the @@ text itself is dropped.
        let ops = parse_patch(
            "*** Begin Patch\n*** Update File: a.rs\n@@ fn one\n-a\n+b\n@@ fn two\n-c\n+d\n*** End Patch",
        )
        .expect("ok");
        match &ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks.len(), 2);
                assert_eq!(hunks[0].old_text, "a");
                assert_eq!(hunks[1].new_text, "d");
            }
            other => panic!("expected update, got {other:?}"),
        }
    }

    #[test]
    fn parse_delete_file_takes_no_body() {
        let ops =
            parse_patch("*** Begin Patch\n*** Delete File: dead.rs\n*** End Patch").expect("ok");
        assert_eq!(
            ops,
            vec![FileOp::Delete {
                path: "dead.rs".into()
            }]
        );
    }

    #[test]
    fn parse_patch_rejects_unknown_star_marker() {
        let e = parse_patch("*** Begin Patch\n*** Rename File: a -> b\n*** End Patch")
            .expect_err("unknown marker");
        assert!(matches!(e, ToolError::ExecutionFailed(m) if m.contains("unexpected line")));
    }

    #[test]
    fn parse_patch_rejects_update_body_line_without_recognized_prefix() {
        let e = parse_patch("*** Begin Patch\n*** Update File: a.rs\nbroken line\n*** End Patch")
            .expect_err("bad prefix");
        assert!(matches!(e, ToolError::ExecutionFailed(m) if m.contains("must start with")));
    }

    #[tokio::test]
    async fn duplicate_target_path_across_sections_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let abs = dir.path().join("a.rs");
        std::fs::write(&abs, "x\n").expect("seed");

        // Two sections targeting the same resolved path. Without the
        // guard the second write would silently clobber the first; the
        // guard fires before any per-op validation or write.
        let resolved = vec![
            (
                FileOp::Delete {
                    path: "a.rs".into(),
                },
                abs.clone(),
            ),
            (
                FileOp::Delete {
                    path: "a.rs".into(),
                },
                abs.clone(),
            ),
        ];
        let cancel = tokio_util::sync::CancellationToken::new();
        let e = apply_under_lock(resolved, &cancel, false)
            .await
            .expect_err("duplicate path must be rejected");
        let ToolError::ExecutionFailed(message) = &e else {
            panic!("expected ExecutionFailed, got {e:?}");
        };
        assert!(message.contains("more than one patch section"), "{message}");
        // The guard is a pure no-op: the file is untouched.
        assert!(abs.exists());
    }
}
