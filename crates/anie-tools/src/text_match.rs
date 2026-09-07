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

/// Maximum number of file lines surfaced in a [`ClosestMatch`] region.
/// Plan 03 §2c caps grounded edit-failure attachments so a stuck model
/// gets enough context to fix its `oldText` without bloating the failed
/// result with the whole file.
pub(crate) const MAX_CLOSEST_MATCH_LINES: usize = 80;

/// The file region most similar to a failed-edit `old_text`, located with
/// the same whitespace-insensitive comparison the fuzzy matcher uses.
///
/// Plan 03 §2c groundwork: when an edit's `old_text` matches nothing, the
/// failure carries this so a later agent-loop wave can show the model the
/// bytes it should have targeted. `start_line`/`end_line` are 1-based and
/// inclusive; `region` is the excerpt with `path:line` markers, capped at
/// [`MAX_CLOSEST_MATCH_LINES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClosestMatch {
    pub(crate) path: String,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) region: String,
}

impl ClosestMatch {
    /// `path:start-end` — the short locator embedded in the failure
    /// message so existing error text stays a prefix and only gains a
    /// suffix.
    pub(crate) fn locator(&self) -> String {
        format!("{}:{}-{}", self.path, self.start_line, self.end_line)
    }
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
            let mut message = format!("edit #{index} for {path} did not match anything");
            // Append the closest region locator so the model (or a later
            // grounding wave) knows where to look. The existing message
            // stays a prefix — only a suffix is added — so current tests
            // that match on "did not match anything" keep passing.
            if let Some(closest) = closest_match_region(content, &edit.old_text, path) {
                message.push_str(&format!("; closest match: {}", closest.locator()));
            }
            return Err(ToolError::ExecutionFailed(message));
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

/// Find the file region in `content` most similar to `needle`, scoring a
/// sliding window of lines by per-line, whitespace-insensitive token
/// overlap (so a region that differs by only a few tokens beats an
/// unrelated one). Returns `None` only when `content` is empty. The
/// returned region is capped at [`MAX_CLOSEST_MATCH_LINES`] and carries
/// `path:line` markers.
pub(crate) fn closest_match_region(
    content: &str,
    needle: &str,
    path: &str,
) -> Option<ClosestMatch> {
    let content_lines: Vec<&str> = content.lines().collect();
    if content_lines.is_empty() {
        return None;
    }
    let needle_lines: Vec<&str> = needle.lines().collect();

    // Window the file in chunks the size of the needle and score each
    // window by per-line token overlap (not strict equality) so a region
    // that differs only by a few tokens scores higher than an unrelated
    // one — this is what makes the result the *closest* match rather than
    // just the first. A zero-line needle (or one that's all blank) still
    // resolves to a single-line window so the model gets a concrete
    // anchor.
    let window = needle_lines.len().clamp(1, content_lines.len());
    let needle_tokens: Vec<Vec<String>> = needle_lines.iter().map(|l| line_tokens(l)).collect();

    let mut best_start = 0usize;
    let mut best_score = -1.0f64;
    for start in 0..=(content_lines.len() - window) {
        let mut score = 0.0f64;
        for offset in 0..window {
            if let Some(needle_line) = needle_tokens.get(offset)
                && !needle_line.is_empty()
            {
                score += line_overlap(&line_tokens(content_lines[start + offset]), needle_line);
            }
        }
        // Earliest window wins ties so the locator is stable.
        if score > best_score {
            best_score = score;
            best_start = start;
        }
    }

    // Cap the surfaced excerpt; the scoring window may be larger than the
    // attachment budget for a big multi-line oldText. `end_line` tracks
    // the shown range so the locator and the region agree.
    let region_end = (best_start + window.min(MAX_CLOSEST_MATCH_LINES)).min(content_lines.len());
    let mut region = String::new();
    for (offset, line) in content_lines[best_start..region_end].iter().enumerate() {
        region.push_str(&format!("{}:{}: {}\n", path, best_start + offset + 1, line));
    }

    Some(ClosestMatch {
        path: path.to_string(),
        start_line: best_start + 1,
        end_line: region_end,
        region: region.trim_end_matches('\n').to_string(),
    })
}

/// Whitespace-split tokens of a single line — the unit the closest-match
/// scorer compares, so two lines that differ only in spacing or a few
/// tokens still register as related.
fn line_tokens(line: &str) -> Vec<String> {
    line.split_whitespace().map(str::to_string).collect()
}

/// Fraction of `needle`'s tokens present in `file` (order-insensitive),
/// in `0.0..=1.0`. An empty `needle` scores 0 so blank needle lines don't
/// inflate a window.
fn line_overlap(file: &[String], needle: &[String]) -> f64 {
    if needle.is_empty() {
        return 0.0;
    }
    let matched = needle.iter().filter(|tok| file.contains(tok)).count();
    matched as f64 / needle.len() as f64
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

    #[test]
    fn failed_match_error_carries_closest_region_with_line_markers() {
        let content = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        // oldText resembles the `beta` line but isn't present verbatim or
        // fuzzily; the failure should point at the closest region.
        let err = apply_edits(content, &[edit("fn beta() { return; }", "x")], "src/lib.rs")
            .expect_err("no match");
        let ToolError::ExecutionFailed(message) = err else {
            panic!("expected ExecutionFailed");
        };
        // Existing wording stays a prefix; only a locator suffix is added.
        assert!(message.starts_with("edit #0 for src/lib.rs did not match anything"));
        assert!(
            message.contains("closest match: src/lib.rs:2-2"),
            "message was: {message}"
        );

        let closest = closest_match_region(content, "fn beta() { return; }", "src/lib.rs")
            .expect("closest region");
        assert_eq!(closest.start_line, 2);
        assert_eq!(closest.end_line, 2);
        assert_eq!(closest.region, "src/lib.rs:2: fn beta() {}");
    }

    #[test]
    fn region_payload_caps_at_eighty_lines() {
        // A 200-line file with a 120-line needle: the scoring window is
        // huge, but the surfaced region must never exceed the cap.
        let content: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        let needle: String = (1..=120).map(|n| format!("line {n} changed\n")).collect();

        let closest = closest_match_region(&content, &needle, "big.txt").expect("closest region");
        let region_lines = closest.region.lines().count();
        assert_eq!(region_lines, MAX_CLOSEST_MATCH_LINES);
        // end_line - start_line + 1 must agree with the shown line count.
        assert_eq!(
            closest.end_line - closest.start_line + 1,
            MAX_CLOSEST_MATCH_LINES
        );
        for (offset, line) in closest.region.lines().enumerate() {
            assert!(
                line.starts_with(&format!("big.txt:{}: ", closest.start_line + offset)),
                "line marker malformed: {line}"
            );
        }
    }

    #[test]
    fn exact_match_success_path_unchanged() {
        // A successful edit must not invoke closest-match machinery or
        // alter its output in any way.
        let outcome = apply_edits("alpha\nbeta\ngamma", &[edit("beta", "BETA")], "f").expect("ok");
        assert_eq!(outcome.updated, "alpha\nBETA\ngamma");
        assert_eq!(outcome.kinds, vec![MatchKind::Exact]);
    }
}
