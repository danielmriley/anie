//! `apply_patch` tool: apply a Codex-style patch envelope carrying
//! multi-hunk, multi-file changes.
//!
//! This module is built in stages: PR3 is the pure, IO-free parser
//! (`parse_patch`) that lowers an envelope into a `Vec<FileOp>`; PR4
//! adds the applier, the `Tool` impl, and multi-file atomic writes.
//! See `docs/apply_patch_tool/README.md`.
// PR3 ships the parser only; the applier (PR4) is the first non-test
// consumer of these items. Remove this once PR4 lands.
#![cfg_attr(not(test), allow(dead_code))]

use anie_agent::ToolError;

use crate::text_match::Edit;

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
}
