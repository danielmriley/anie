//! Fuzzy-scoring helper used by the model picker.
//!
//! OpenRouter's live catalog is 500+ entries, so a pure substring
//! search produces long unhelpful result lists (e.g. typing
//! "claude" matches every `anthropic/claude-*` variant in arbitrary
//! order). This module scores a candidate against a query so the
//! picker can sort best-first.
//!
//! # Scoring shape
//!
//! Higher score = better match. Returns `None` when the candidate
//! doesn't match at all. The scorer is case-insensitive.
//!
//! Scores are layered by the strength of the match:
//!
//! | Tier            | Base score | Example (query `claude`)         |
//! |-----------------|------------|----------------------------------|
//! | Exact           | 1_000_000  | `claude`                         |
//! | Prefix          |   500_000  | `claude-sonnet-4`                |
//! | Word-start      |   250_000  | `anthropic/claude` (starts word) |
//! | Contiguous      |   100_000  | `new-claude-preview`             |
//! | Subsequence     |    10_000  | `cl-and-u-de`                    |
//!
//! Within a tier, earlier matches and shorter candidates score
//! higher. A match at position 0 adds a large bonus; longer
//! candidates decay the score slightly so `claude-3-haiku` beats
//! `claude-3-sonnet-20250219-custom-tuned` for the same query.
//!
//! Word separators recognized: `/`, `-`, `_`, `.`, `:`, space.
//! These match the delimiters OpenRouter and other aggregators use
//! in model ids.

/// Score a candidate against a lowercased query.
///
/// Returns `None` when the candidate doesn't match. Higher score =
/// better match; callers sort descending by score.
#[must_use]
pub(crate) fn fuzzy_score(query: &str, candidate: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let query_lower = query.to_ascii_lowercase();
    let candidate_lower = candidate.to_ascii_lowercase();

    if candidate_lower == query_lower {
        return Some(scored(1_000_000, candidate_lower.len(), 0));
    }
    if let Some(position) = candidate_lower.find(&query_lower) {
        if position == 0 {
            return Some(scored(500_000, candidate_lower.len(), 0));
        }
        if starts_word_at(&candidate_lower, position) {
            return Some(scored(250_000, candidate_lower.len(), position));
        }
        return Some(scored(100_000, candidate_lower.len(), position));
    }

    subsequence_score(&query_lower, &candidate_lower)
}

/// `true` when the byte-position `position` in `text` sits at the
/// start of a word (position 0, or the previous byte is a
/// recognized separator). ASCII-only — this module consumes
/// already-lowercased ASCII input.
fn starts_word_at(text: &str, position: usize) -> bool {
    if position == 0 {
        return true;
    }
    let prev = text.as_bytes()[position - 1];
    matches!(prev, b'/' | b'-' | b'_' | b'.' | b':' | b' ')
}

/// Walk the candidate left-to-right consuming each query char in
/// order. Returns `None` when any query char is missing. Rewards
/// query chars that land at word starts — matches the plan's
/// `a/c` → `anthropic/claude` scenario.
fn subsequence_score(query: &str, candidate: &str) -> Option<u32> {
    let mut query_iter = query.chars();
    let mut current = query_iter.next()?;
    let mut score: u32 = 10_000;
    let mut word_boundary_hits: u32 = 0;
    let mut last_index: Option<usize> = None;

    for (index, ch) in candidate.char_indices() {
        if ch == current {
            if starts_word_at(candidate, index) {
                word_boundary_hits = word_boundary_hits.saturating_add(1);
            }
            if let Some(last) = last_index
                && index == last + ch.len_utf8()
            {
                // Contiguous-within-subsequence runs are worth a
                // small bump so "anclaude" still ranks above
                // scatter-matches across the string.
                score = score.saturating_add(50);
            }
            last_index = Some(index);
            match query_iter.next() {
                Some(next) => current = next,
                None => {
                    score = score.saturating_add(word_boundary_hits * 500);
                    score = score.saturating_sub(
                        u32::try_from(candidate.len()).unwrap_or(u32::MAX).min(5_000),
                    );
                    return Some(score);
                }
            }
        }
    }
    None
}

/// Apply the shared length + position decay for tiered matches.
fn scored(base: u32, candidate_len: usize, match_position: usize) -> u32 {
    let length_decay = u32::try_from(candidate_len).unwrap_or(u32::MAX).min(5_000);
    let position_decay = u32::try_from(match_position * 10)
        .unwrap_or(u32::MAX)
        .min(5_000);
    base.saturating_sub(length_decay).saturating_sub(position_decay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, candidate: &str) -> Option<u32> {
        fuzzy_score(query, candidate)
    }

    #[test]
    fn empty_query_scores_zero_for_every_candidate() {
        assert_eq!(score("", "anthropic/claude-sonnet-4"), Some(0));
        assert_eq!(score("", ""), Some(0));
    }

    #[test]
    fn no_match_returns_none() {
        assert_eq!(score("xyz", "anthropic/claude-sonnet-4"), None);
    }

    #[test]
    fn exact_match_beats_prefix_match() {
        let exact = score("claude", "claude").expect("exact");
        let prefix = score("claude", "claude-sonnet-4").expect("prefix");
        assert!(
            exact > prefix,
            "expected exact ({exact}) > prefix ({prefix})"
        );
    }

    #[test]
    fn prefix_match_beats_word_start_match() {
        let prefix = score("claude", "claude-3-haiku").expect("prefix");
        let word_start = score("claude", "anthropic/claude-3-haiku").expect("word-start");
        assert!(
            prefix > word_start,
            "expected prefix ({prefix}) > word-start ({word_start})"
        );
    }

    #[test]
    fn word_start_match_beats_mid_word_substring() {
        let word_start = score("claude", "anthropic/claude-3-haiku").expect("word-start");
        let mid_word = score("laude", "applaude").expect("mid-word");
        assert!(
            word_start > mid_word,
            "expected word-start ({word_start}) > mid-word ({mid_word})"
        );
    }

    #[test]
    fn contiguous_substring_beats_sparse_subsequence() {
        let contiguous = score("sonnet", "anthropic/claude-sonnet").expect("contiguous");
        let sparse = score("sonnet", "so-and-non-net-extra").expect("sparse");
        assert!(
            contiguous > sparse,
            "expected contiguous ({contiguous}) > sparse ({sparse})"
        );
    }

    #[test]
    fn subsequence_with_word_boundary_hits_beats_mid_word_subsequence() {
        // Both matches land in the subsequence tier (no contiguous
        // substring of the query appears). `acs` hits two word
        // starts in `anthropic/claude-sonnet` (`a` at 0 and `s`
        // after `-`), versus one in `accessibles`. The multi-
        // boundary match should score higher even though the
        // candidate is longer.
        let word_starts = score("acs", "anthropic/claude-sonnet").expect("word-starts");
        let mid_word = score("acs", "accessibles").expect("mid-word");
        assert!(
            word_starts > mid_word,
            "expected word-starts ({word_starts}) > mid-word ({mid_word})"
        );
    }

    #[test]
    fn shorter_candidate_beats_longer_at_same_tier() {
        let shorter = score("claude", "claude-3-haiku").expect("shorter");
        let longer = score(
            "claude",
            "claude-3-sonnet-20250219-custom-tuned-instruct-long",
        )
        .expect("longer");
        assert!(
            shorter > longer,
            "expected shorter ({shorter}) > longer ({longer})"
        );
    }

    #[test]
    fn earlier_position_beats_later_position_at_same_tier() {
        // Both mid-word contiguous matches (not at word starts):
        // "claude" follows `x` in both cases, so neither earns the
        // word-start tier. The one where "claude" appears sooner
        // wins via the position-decay term.
        let early = score("claude", "xclaude-variant").expect("early");
        let late = score("claude", "xabcdefghijklmnopqrstuvxclaude").expect("late");
        assert!(early > late, "expected early ({early}) > late ({late})");
    }

    #[test]
    fn case_insensitive() {
        assert!(score("CLAUDE", "anthropic/claude-sonnet").is_some());
        assert!(score("claude", "ANTHROPIC/CLAUDE-SONNET").is_some());
    }

    #[test]
    fn respects_all_configured_word_separators() {
        for sep in ['/', '-', '_', '.', ':', ' '] {
            let candidate = format!("provider{sep}claude-3");
            let s = score("claude", &candidate);
            assert!(
                s.is_some(),
                "separator {sep:?} should mark a word start"
            );
        }
    }
}
