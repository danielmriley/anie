//! Shared text-matching engine for `edit` and `apply_patch`.
//!
//! Locates each replacement block in a file with an exact pass and a
//! whitespace-tolerant fuzzy fallback, rejects ambiguous and overlapping
//! matches, and applies the surviving edits right-to-left so earlier byte
//! offsets stay valid. Extracted verbatim from `edit.rs` (no behavior
//! change) so both tools share one battle-tested matcher; the only
//! addition is [`MatchKind`], which lets callers report whether a match
//! was exact or fuzzy (the EDIT-3 transparency signal).

use anie_agent::ToolError;
use similar::{ChangeTag, TextDiff};

/// One replacement: find `old_text`, swap in `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    pub(crate) old_text: String,
    pub(crate) new_text: String,
}

/// How an edit's `old_text` was located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchKind {
    /// Matched verbatim.
    Exact,
    /// Matched only after whitespace normalization.
    Fuzzy,
}

/// Result of applying a batch of edits to one file's (LF-normalized)
/// content. `kinds` is in input-edit order so callers can report which
/// edits matched fuzzily.
#[derive(Debug, Clone)]
pub(crate) struct ApplyOutcome {
    pub(crate) updated: String,
    /// Per-edit, in input order: whether the match was exact or fuzzy.
    /// Consumed by `edit` and `apply_patch` for fuzzy-match reporting.
    pub(crate) kinds: Vec<MatchKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MatchedEdit {
    edit_index: usize,
    start: usize,
    end: usize,
    new_text: String,
}

/// Match every edit against `content` and apply them, returning the new
/// content plus per-edit [`MatchKind`]. Errors (empty `old_text`,
/// ambiguous match, no match, overlap) carry the edit index and `path`
/// for a model-actionable message — identical wording to the original
/// `edit` engine.
pub(crate) fn apply_edits(
    content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<ApplyOutcome, ToolError> {
    let mut matched = Vec::with_capacity(edits.len());
    let mut kinds = Vec::with_capacity(edits.len());
    // Lazily compute the normalized content + index map at most once per
    // edit batch. Most batches hit the exact-match fast path on every
    // edit and never need fuzzy normalization; the first fuzzy fallback
    // materializes the cache, and subsequent fuzzy edits reuse it.
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
            kinds.push(MatchKind::Exact);
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
        kinds.push(MatchKind::Fuzzy);
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
    Ok(ApplyOutcome { updated, kinds })
}

/// Render a unified-style diff between two strings (`similar`-backed).
pub(crate) fn render_diff(original: &str, updated: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn text_match_reports_exact_when_old_text_matches_verbatim() {
        let outcome = apply_edits("alpha beta gamma", &[edit("beta", "BETA")], "f").expect("ok");
        assert_eq!(outcome.updated, "alpha BETA gamma");
        assert_eq!(outcome.kinds, vec![MatchKind::Exact]);
    }

    #[test]
    fn text_match_reports_fuzzy_when_only_whitespace_normalized_match_exists() {
        // The needle differs only in internal whitespace from the file.
        let outcome =
            apply_edits("let  x   =  1;", &[edit("let x = 1;", "let x = 2;")], "f").expect("ok");
        assert_eq!(outcome.updated, "let x = 2;");
        assert_eq!(outcome.kinds, vec![MatchKind::Fuzzy]);
    }

    #[test]
    fn text_match_rejects_ambiguous_exact_match_with_multiple_regions() {
        let err = apply_edits("x x x", &[edit("x", "y")], "f").expect_err("ambiguous");
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("matched 3 regions")));
    }

    #[test]
    fn text_match_rejects_overlapping_edits() {
        let err =
            apply_edits("abcdef", &[edit("abc", "X"), edit("bcd", "Y")], "f").expect_err("overlap");
        assert!(matches!(err, ToolError::ExecutionFailed(m) if m.contains("overlaps")));
    }
}
