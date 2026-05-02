//! Parallel-decomposition plan parser. PR 5 of
//! `docs/rlm_subagents_2026-05-01/`.
//!
//! Reads a plan produced by `Decomposer::decompose` (PR 4),
//! parses dependency markers like `(depends on 1)` or
//! `(after 2, 3)`, and groups sub-tasks into topological
//! rounds. Sub-tasks within a round are independent and
//! could run concurrently; rounds run sequentially.
//!
//! This module is the **structure** layer. The actual
//! parallel-execution layer is PR 5.1 — once smoke proves
//! the dry-run round structure is useful, we wire an
//! executor that fans out via `ControllerSubAgentFactory`
//! and runs the rounds concurrently.
//!
//! ## Parsing rules (lenient by design)
//!
//! - Numbered lines like `1.`, `2.`, `1)` are sub-tasks.
//! - A `(depends on N)` or `(after N, M)` suffix marks
//!   dependencies on prior sub-task IDs.
//! - Lines without a marker are assumed independent.
//! - Bullet-point or prose-only plans skip parsing — caller
//!   gets `None` and falls back to PR 4's plain text plan.
//! - Cycles or missing dependencies → `None` (sequential
//!   fallback). Never panic on malformed input; never
//!   block the user's turn on a parse failure.

#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeSet, HashMap};

use regex::Regex;

/// One sub-task parsed from the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subtask {
    pub id: u32,
    pub text: String,
    pub depends_on: Vec<u32>,
}

/// Parsed plan + computed rounds. `rounds[i]` is a Vec of
/// sub-task IDs that can all run concurrently; `rounds[i+1]`
/// runs after `rounds[i]` completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedPlan {
    pub subtasks: Vec<Subtask>,
    pub rounds: Vec<Vec<u32>>,
}

/// Parse a plan into structured sub-tasks and topological
/// rounds. Returns `None` on any parse failure (no
/// numbered lines, cycle, dangling dependency reference) so
/// callers can gracefully fall back to PR 4's plain plan.
pub(crate) fn parse_plan(plan: &str) -> Option<ParsedPlan> {
    let subtasks = extract_subtasks(plan);
    if subtasks.is_empty() {
        return None;
    }
    if !dependencies_resolve(&subtasks) {
        return None;
    }
    let rounds = topological_rounds(&subtasks)?;
    Some(ParsedPlan { subtasks, rounds })
}

/// Extract numbered sub-tasks from the plan text. Recognizes
/// `1.` and `1)` as line markers. Strips the marker and
/// trims; uses [`extract_dependencies`] on the remaining
/// text to pull out any `(depends on ...)` / `(after ...)`
/// suffix.
fn extract_subtasks(plan: &str) -> Vec<Subtask> {
    // SAFETY: the pattern is a literal that compiles
    // successfully — verified by the unit tests below. If
    // somehow it fails at runtime, we still want to behave
    // gracefully (return empty subtasks, which the caller
    // treats as "no plan structure" → fall through to the
    // PR 4 plain-text plan).
    let Ok(line_re) = Regex::new(r"^\s*(\d+)[.)]\s+(.*)$") else {
        return Vec::new();
    };
    let mut subtasks = Vec::new();
    for line in plan.lines() {
        if let Some(caps) = line_re.captures(line) {
            let id: u32 = caps[1].parse().unwrap_or(0);
            if id == 0 {
                continue;
            }
            let body = caps[2].trim().to_string();
            let (text, depends_on) = extract_dependencies(&body);
            subtasks.push(Subtask {
                id,
                text,
                depends_on,
            });
        }
    }
    subtasks
}

/// Parse out dependency markers from a sub-task line body.
/// Recognizes parenthesized `depends on` / `after` /
/// `requires` followed by comma-separated integers.
/// Returns `(text_without_marker, dep_ids)`.
fn extract_dependencies(body: &str) -> (String, Vec<u32>) {
    let Ok(dep_re) = Regex::new(r"(?i)\((?:depends\s+on|after|requires)\s+([\d,\s]+)\)") else {
        return (body.to_string(), Vec::new());
    };
    if let Some(caps) = dep_re.captures(body) {
        let raw_ids = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let ids: Vec<u32> = raw_ids
            .split(',')
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .collect();
        let text_without = dep_re.replace(body, "").trim().to_string();
        return (text_without, ids);
    }
    (body.to_string(), Vec::new())
}

/// Confirm every dependency points at an actual prior
/// sub-task. Missing references = invalid plan.
fn dependencies_resolve(subtasks: &[Subtask]) -> bool {
    let known: BTreeSet<u32> = subtasks.iter().map(|s| s.id).collect();
    for s in subtasks {
        for dep in &s.depends_on {
            if !known.contains(dep) {
                return false;
            }
        }
    }
    true
}

/// Group sub-tasks into topological rounds. Each round is a
/// Vec of IDs that can all run concurrently. Returns `None`
/// on cycle.
fn topological_rounds(subtasks: &[Subtask]) -> Option<Vec<Vec<u32>>> {
    let by_id: HashMap<u32, &Subtask> = subtasks.iter().map(|s| (s.id, s)).collect();
    let mut remaining: BTreeSet<u32> = subtasks.iter().map(|s| s.id).collect();
    let mut completed: BTreeSet<u32> = BTreeSet::new();
    let mut rounds: Vec<Vec<u32>> = Vec::new();
    while !remaining.is_empty() {
        let ready: Vec<u32> = remaining
            .iter()
            .filter(|id| {
                by_id
                    .get(id)
                    .is_some_and(|st| st.depends_on.iter().all(|d| completed.contains(d)))
            })
            .copied()
            .collect();
        if ready.is_empty() {
            // Cycle.
            return None;
        }
        for id in &ready {
            remaining.remove(id);
            completed.insert(*id);
        }
        rounds.push(ready);
    }
    Some(rounds)
}

/// Render a parsed plan as an annotated text injection. PR 5
/// dry-run mode — surfaces the round structure so the model
/// (and the user, via the existing transcript SystemMessage
/// in PR 4) can see which sub-tasks could run in parallel.
/// Even without a parallel executor (PR 5.1), the model can
/// see "Steps 2 and 3 are independent — consider tackling
/// them in either order or in parallel via separate recurse
/// calls."
pub(crate) fn render_with_rounds(plan: &ParsedPlan) -> String {
    let mut out = String::new();
    for subtask in &plan.subtasks {
        let dep_note = if subtask.depends_on.is_empty() {
            String::new()
        } else {
            let deps: Vec<String> =
                subtask.depends_on.iter().map(|id| id.to_string()).collect();
            format!(" (depends on {})", deps.join(", "))
        };
        out.push_str(&format!("{}. {}{}\n", subtask.id, subtask.text, dep_note));
    }
    out.push('\n');
    if plan.rounds.len() > 1 {
        out.push_str("Parallel structure:\n");
        for (idx, round) in plan.rounds.iter().enumerate() {
            let ids: Vec<String> = round.iter().map(|id| id.to_string()).collect();
            let parallel_note = if round.len() > 1 {
                " (independent — could run in parallel)"
            } else {
                ""
            };
            out.push_str(&format!(
                "  Round {}: [{}]{}\n",
                idx + 1,
                ids.join(", "),
                parallel_note
            ));
        }
    } else if let Some(round) = plan.rounds.first()
        && round.len() > 1
    {
        out.push_str(&format!(
            "All {} sub-tasks are independent — could run in any order or in parallel.\n",
            round.len()
        ));
    }
    out
}

/// True when `ANIE_PARALLEL_DECOMPOSE` is set to a value of
/// 2 or higher. The value indicates the (future) max
/// concurrency; for the dry-run iteration any value of 2 or
/// higher just enables the round-rendering.
pub(crate) fn parallel_decompose_enabled() -> bool {
    std::env::var("ANIE_PARALLEL_DECOMPOSE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .is_some_and(|n| n >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_extracts_independent_subtasks() {
        let plan = "1. Implement parser\n2. Write tests\n3. Run tests";
        let parsed = parse_plan(plan).expect("parse");
        assert_eq!(parsed.subtasks.len(), 3);
        for s in &parsed.subtasks {
            assert!(s.depends_on.is_empty(), "{s:?}");
        }
        assert_eq!(parsed.rounds.len(), 1);
        assert_eq!(parsed.rounds[0], vec![1, 2, 3]);
    }

    #[test]
    fn parser_recognises_depends_on_single_task() {
        let plan = "1. Build header\n2. Write driver (depends on 1)\n3. Compile (depends on 2)";
        let parsed = parse_plan(plan).expect("parse");
        assert_eq!(parsed.subtasks[0].depends_on, Vec::<u32>::new());
        assert_eq!(parsed.subtasks[1].depends_on, vec![1]);
        assert_eq!(parsed.subtasks[2].depends_on, vec![2]);
        // Three sequential rounds.
        assert_eq!(parsed.rounds, vec![vec![1], vec![2], vec![3]]);
    }

    #[test]
    fn parser_recognises_after_marker() {
        let plan = "1. A\n2. B\n3. C (after 1, 2)";
        let parsed = parse_plan(plan).expect("parse");
        assert_eq!(parsed.subtasks[2].depends_on, vec![1, 2]);
        assert_eq!(parsed.rounds, vec![vec![1, 2], vec![3]]);
    }

    #[test]
    fn parser_recognises_requires_marker() {
        let plan = "1. A\n2. B (requires 1)";
        let parsed = parse_plan(plan).expect("parse");
        assert_eq!(parsed.subtasks[1].depends_on, vec![1]);
    }

    #[test]
    fn parser_handles_mixed_independent_and_dependent() {
        // Diamond: 1 → 2, 1 → 3, 2,3 → 4.
        let plan = "1. Setup\n2. Branch A (depends on 1)\n3. Branch B (depends on 1)\n4. Merge (depends on 2, 3)";
        let parsed = parse_plan(plan).expect("parse");
        assert_eq!(parsed.rounds.len(), 3);
        assert_eq!(parsed.rounds[0], vec![1]);
        assert_eq!(parsed.rounds[1], vec![2, 3]);
        assert_eq!(parsed.rounds[2], vec![4]);
    }

    #[test]
    fn parser_returns_none_on_cycle() {
        let plan = "1. A (depends on 2)\n2. B (depends on 1)";
        assert!(parse_plan(plan).is_none(), "cycle should fail to parse");
    }

    #[test]
    fn parser_returns_none_on_dangling_dependency() {
        let plan = "1. A\n2. B (depends on 99)";
        assert!(
            parse_plan(plan).is_none(),
            "dangling dep should fail to parse"
        );
    }

    #[test]
    fn parser_returns_none_on_no_numbered_lines() {
        let plan = "Just some prose, no numbered list here.";
        assert!(parse_plan(plan).is_none());
    }

    #[test]
    fn parser_handles_paren_form_numbering() {
        let plan = "1) First\n2) Second";
        let parsed = parse_plan(plan).expect("parse");
        assert_eq!(parsed.subtasks.len(), 2);
    }

    #[test]
    fn parser_strips_dep_marker_from_sub_task_text() {
        let plan = "1. First\n2. Second thing (depends on 1) more text after";
        let parsed = parse_plan(plan).expect("parse");
        // Marker stripped. "more text after" survives because
        // it's outside the parens.
        assert!(
            !parsed.subtasks[1].text.contains("depends on"),
            "{}",
            parsed.subtasks[1].text
        );
        assert!(
            parsed.subtasks[1].text.contains("more text after"),
            "{}",
            parsed.subtasks[1].text
        );
    }

    #[test]
    fn render_with_rounds_shows_parallel_groups() {
        let plan = "1. A\n2. B\n3. C (depends on 1)";
        let parsed = parse_plan(plan).expect("parse");
        let rendered = render_with_rounds(&parsed);
        assert!(
            rendered.contains("Parallel structure"),
            "missing parallel section: {rendered}"
        );
        assert!(
            rendered.contains("could run in parallel"),
            "missing parallel hint: {rendered}"
        );
    }

    #[test]
    fn render_with_rounds_collapses_single_round_note() {
        let plan = "1. A\n2. B\n3. C";
        let parsed = parse_plan(plan).expect("parse");
        let rendered = render_with_rounds(&parsed);
        assert!(
            !rendered.contains("Round 1"),
            "single round shouldn't render numbered round list: {rendered}"
        );
        assert!(
            rendered.contains("All 3 sub-tasks are independent"),
            "missing all-independent note: {rendered}"
        );
    }

    #[test]
    fn parallel_decompose_enabled_requires_at_least_two() {
        let cases = [("", false), ("0", false), ("1", false), ("2", true), ("4", true)];
        for (value, expected) in cases {
            // SAFETY: env mutation in tests; this crate runs
            // single-threaded for env-mutating tests.
            unsafe {
                if value.is_empty() {
                    std::env::remove_var("ANIE_PARALLEL_DECOMPOSE");
                } else {
                    std::env::set_var("ANIE_PARALLEL_DECOMPOSE", value);
                }
            }
            assert_eq!(parallel_decompose_enabled(), expected, "value={value}");
        }
        unsafe {
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE");
        }
    }
}
