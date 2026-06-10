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
/// 2 or higher. Enables both the dry-run round annotation
/// (PR 5) and the concurrent executor (PR 5.1) — the
/// executor's effective concurrency is further clamped by
/// [`safe_max_concurrency`] based on the parent's API.
pub(crate) fn parallel_decompose_enabled() -> bool {
    std::env::var("ANIE_PARALLEL_DECOMPOSE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .is_some_and(|n| n >= 2)
}

/// Hard cap on concurrent sub-agent execution. We stay well
/// below typical API rate limits even for users with 4-7
/// sub-tasks per decompose; lifting this requires an
/// `ANIE_PARALLEL_DECOMPOSE_MAX` knob (deferred until
/// someone asks).
pub(crate) const MAX_PARALLEL_DECOMPOSE_CAP: u32 = 6;

/// Provider-aware concurrency selection. PR 5.1 of
/// `docs/rlm_subagents_2026-05-01/`.
///
/// **Ollama defaults to sequential** (`max = 1`) regardless of
/// `ANIE_PARALLEL_DECOMPOSE` because local concurrent inference
/// without shared-prefix KV cache (a) doubles VRAM pressure and
/// (b) re-processes the shared prefix per sub-agent. See
/// `concurrency_decisions.md` for the long-form reasoning.
///
/// `ANIE_PARALLEL_DECOMPOSE_FORCE=1` overrides the Ollama
/// clamp for users who know their hardware can handle it.
///
/// API providers (anything that's not Ollama) honor
/// `ANIE_PARALLEL_DECOMPOSE` directly, capped at
/// [`MAX_PARALLEL_DECOMPOSE_CAP`].
pub(crate) fn safe_max_concurrency(api: anie_provider::ApiKind) -> u32 {
    let raw = std::env::var("ANIE_PARALLEL_DECOMPOSE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    if raw < 2 {
        return 1;
    }
    let bounded = raw.min(MAX_PARALLEL_DECOMPOSE_CAP);
    if api == anie_provider::ApiKind::OllamaChatApi
        && !ollama_force_parallel_enabled()
    {
        return 1;
    }
    bounded
}

fn ollama_force_parallel_enabled() -> bool {
    matches!(
        std::env::var("ANIE_PARALLEL_DECOMPOSE_FORCE").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

// ---- PR 5.1: concurrent executor ----

/// Result of executing one sub-task. Errors don't fail the
/// whole plan — they're captured here and surfaced in the
/// composed text so the parent agent can decide whether to
/// retry, skip, or continue.
#[derive(Debug, Clone)]
pub(crate) struct SubtaskResult {
    pub id: u32,
    pub text: String,
    pub answer: Option<String>,
    pub error: Option<String>,
}

/// Drive the parsed plan through topological rounds, running
/// sub-tasks within each round concurrently up to
/// `max_concurrency`. Sequential between rounds (downstream
/// rounds depend on upstream rounds completing).
///
/// Returns one `SubtaskResult` per sub-task in the plan,
/// keyed by id, in plan order.
pub(crate) async fn execute_parallel_plan(
    parsed: &ParsedPlan,
    factory: std::sync::Arc<dyn anie_agent::SubAgentFactory>,
    parent_context: Vec<anie_protocol::Message>,
    recursion_budget: std::sync::Arc<std::sync::atomic::AtomicU32>,
    max_concurrency: u32,
    cancel: &tokio_util::sync::CancellationToken,
) -> Vec<SubtaskResult> {
    use std::collections::HashMap;
    use tokio::sync::Semaphore;

    let mut results: HashMap<u32, SubtaskResult> = HashMap::new();
    let semaphore = std::sync::Arc::new(Semaphore::new(max_concurrency.max(1) as usize));
    let id_to_text: HashMap<u32, String> = parsed
        .subtasks
        .iter()
        .map(|s| (s.id, s.text.clone()))
        .collect();

    for round in &parsed.rounds {
        if cancel.is_cancelled() {
            break;
        }
        // Spawn one task per sub-task in this round, gated
        // by the semaphore.
        let mut handles: Vec<tokio::task::JoinHandle<SubtaskResult>> =
            Vec::with_capacity(round.len());
        for &id in round {
            let Some(text) = id_to_text.get(&id).cloned() else {
                continue;
            };
            let factory = std::sync::Arc::clone(&factory);
            let context = parent_context.clone();
            let recursion_budget = std::sync::Arc::clone(&recursion_budget);
            let cancel = cancel.clone();
            let permit_acquirer = std::sync::Arc::clone(&semaphore);
            handles.push(tokio::spawn(async move {
                let _permit = permit_acquirer.acquire_owned().await.ok();
                run_one_subtask(
                    id,
                    &text,
                    factory,
                    context,
                    recursion_budget,
                    &cancel,
                )
                .await
            }));
        }
        for handle in handles {
            match handle.await {
                Ok(result) => {
                    results.insert(result.id, result);
                }
                Err(join_err) => {
                    tracing::warn!(%join_err, "sub-task task panicked or was aborted");
                }
            }
        }
    }

    // Return in plan order so composition reads naturally.
    parsed
        .subtasks
        .iter()
        .filter_map(|st| results.remove(&st.id))
        .collect()
}

/// Build + drive one sub-agent for a single sub-task. The
/// sub-agent inherits the parent's tool registry (PR 2 of
/// the sub-agents series) and starts with the parent's
/// active context as its initial messages.
async fn run_one_subtask(
    id: u32,
    text: &str,
    factory: std::sync::Arc<dyn anie_agent::SubAgentFactory>,
    parent_context: Vec<anie_protocol::Message>,
    recursion_budget: std::sync::Arc<std::sync::atomic::AtomicU32>,
    cancel: &tokio_util::sync::CancellationToken,
) -> SubtaskResult {
    use anie_protocol::{AgentEvent, ContentBlock, Message, UserMessage, now_millis};

    let build_ctx = anie_agent::SubAgentBuildContext {
        depth: 1,
        recursion_budget,
        model_override: None,
    };
    let sub_agent = match factory.build(&build_ctx) {
        Ok(a) => a,
        Err(error) => {
            return SubtaskResult {
                id,
                text: text.to_string(),
                answer: None,
                error: Some(format!("sub-agent build failed: {error}")),
            };
        }
    };

    let user_prompt = Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        timestamp: now_millis(),
    });
    let (sub_event_tx, mut sub_event_rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let drain_task = tokio::spawn(async move {
        while sub_event_rx.recv().await.is_some() {
            // Opaque sub-call: events drained, not surfaced.
        }
    });
    let mut machine = sub_agent
        .start_run_machine(vec![user_prompt], parent_context, &sub_event_tx)
        .await;
    while !machine.is_finished() {
        machine.next_step(&sub_event_tx, cancel).await;
    }
    let sub_result = machine.finish(&sub_event_tx).await;
    drop(sub_event_tx);
    let _ = drain_task.await;

    if let Some(error) = sub_result.terminal_error {
        return SubtaskResult {
            id,
            text: text.to_string(),
            answer: None,
            error: Some(format!("sub-agent terminated: {error}")),
        };
    }

    let answer = extract_final_text(&sub_result.generated_messages);
    SubtaskResult {
        id,
        text: text.to_string(),
        answer: Some(answer),
        error: None,
    }
}

fn extract_final_text(messages: &[anie_protocol::Message]) -> String {
    use anie_protocol::{ContentBlock, Message};
    for message in messages.iter().rev() {
        if let Message::Assistant(a) = message {
            for block in &a.content {
                if let ContentBlock::Text { text } = block
                    && !text.trim().is_empty()
                {
                    return text.trim().to_string();
                }
            }
        }
    }
    String::from("(sub-agent produced no visible answer)")
}

/// Render the executed sub-task results as a system-reminder-
/// tagged block. Replaces the plain plan injection when the
/// concurrent executor ran successfully.
pub(crate) fn render_completed_results(
    parsed: &ParsedPlan,
    results: &[SubtaskResult],
) -> String {
    let mut out = String::from(
        "<system-reminder source=\"decompose-executed\">\n\nSub-tasks have been completed by parallel sub-agents. Their results follow — synthesize them into your final answer for the user. Verify any code/output the sub-tasks produced before reporting success.\n\nPLAN:\n",
    );
    out.push_str(&render_with_rounds(parsed));
    out.push_str("\nSUB-TASK RESULTS:\n");
    for result in results {
        out.push_str(&format!("\n--- Sub-task {} ---\n", result.id));
        out.push_str(&format!("Task: {}\n", result.text));
        match (&result.answer, &result.error) {
            (Some(answer), _) => {
                out.push_str(&format!("Result:\n{answer}\n"));
            }
            (None, Some(err)) => {
                out.push_str(&format!("[error]: {err}\n"));
            }
            (None, None) => {
                out.push_str("[no result]\n");
            }
        }
    }
    out.push_str("\n</system-reminder>");
    out
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

    /// PR 5.1: Ollama clamps to sequential by default.
    /// `ANIE_PARALLEL_DECOMPOSE_FORCE=1` overrides.
    #[test]
    fn safe_max_concurrency_clamps_ollama_to_one_by_default() {
        unsafe {
            std::env::set_var("ANIE_PARALLEL_DECOMPOSE", "4");
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE_FORCE");
        }
        assert_eq!(safe_max_concurrency(anie_provider::ApiKind::OllamaChatApi), 1);
        // API providers honor the env value.
        assert_eq!(safe_max_concurrency(anie_provider::ApiKind::OpenAICompletions), 4);
        unsafe {
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE");
        }
    }

    #[test]
    fn safe_max_concurrency_force_lets_ollama_run_parallel() {
        unsafe {
            std::env::set_var("ANIE_PARALLEL_DECOMPOSE", "3");
            std::env::set_var("ANIE_PARALLEL_DECOMPOSE_FORCE", "1");
        }
        assert_eq!(safe_max_concurrency(anie_provider::ApiKind::OllamaChatApi), 3);
        unsafe {
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE");
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE_FORCE");
        }
    }

    #[test]
    fn safe_max_concurrency_caps_at_max() {
        unsafe {
            std::env::set_var("ANIE_PARALLEL_DECOMPOSE", "100");
        }
        assert_eq!(
            safe_max_concurrency(anie_provider::ApiKind::OpenAICompletions),
            MAX_PARALLEL_DECOMPOSE_CAP
        );
        unsafe {
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE");
        }
    }

    #[test]
    fn safe_max_concurrency_returns_one_when_unset() {
        unsafe {
            std::env::remove_var("ANIE_PARALLEL_DECOMPOSE");
        }
        assert_eq!(safe_max_concurrency(anie_provider::ApiKind::OpenAICompletions), 1);
        assert_eq!(safe_max_concurrency(anie_provider::ApiKind::OllamaChatApi), 1);
    }

    #[test]
    fn render_completed_results_includes_each_subtask_answer() {
        let plan = "1. First task\n2. Second task";
        let parsed = parse_plan(plan).expect("parse");
        let results = vec![
            SubtaskResult {
                id: 1,
                text: "First task".into(),
                answer: Some("Got result A".into()),
                error: None,
            },
            SubtaskResult {
                id: 2,
                text: "Second task".into(),
                answer: Some("Got result B".into()),
                error: None,
            },
        ];
        let rendered = render_completed_results(&parsed, &results);
        assert!(rendered.contains("Got result A"), "{rendered}");
        assert!(rendered.contains("Got result B"), "{rendered}");
        assert!(rendered.contains("decompose-executed"), "{rendered}");
        assert!(rendered.starts_with("<system-reminder"));
        assert!(rendered.ends_with("</system-reminder>"));
    }

    #[test]
    fn render_completed_results_includes_errors_when_subtask_failed() {
        let plan = "1. Working\n2. Failing";
        let parsed = parse_plan(plan).expect("parse");
        let results = vec![
            SubtaskResult {
                id: 1,
                text: "Working".into(),
                answer: Some("ok".into()),
                error: None,
            },
            SubtaskResult {
                id: 2,
                text: "Failing".into(),
                answer: None,
                error: Some("network unavailable".into()),
            },
        ];
        let rendered = render_completed_results(&parsed, &results);
        assert!(rendered.contains("[error]: network unavailable"), "{rendered}");
        assert!(rendered.contains("ok"), "{rendered}");
    }
}
