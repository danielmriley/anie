//! Evidence-based final-answer material.
//!
//! The harness lists *observed* tool and validation results. The
//! model is told to cite only this list. Unobserved checks land
//! under `Not run`.

use anie_protocol::{ContentBlock, Message};

/// One observed tool or validation outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedFact {
    pub kind: ObservedKind,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedKind {
    Done,
    Validation,
}

/// Facts collected from a run's messages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedEvidence {
    pub facts: Vec<ObservedFact>,
}

impl ObservedEvidence {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

/// Walk generated / context messages and record tool outcomes.
#[must_use]
pub fn collect_observed_evidence(messages: &[Message]) -> ObservedEvidence {
    let mut facts = Vec::new();
    for message in messages {
        let Message::ToolResult(result) = message else {
            continue;
        };
        let text = join_text(&result.content);
        let summary = summarize_result(&result.tool_name, result.is_error, &text);
        let kind = classify_result(&result.tool_name, &text);
        facts.push(ObservedFact { kind, summary });
    }
    ObservedEvidence { facts }
}

/// Render the Done / Validation / Not run brief. Only cites
/// facts present in `evidence`.
#[must_use]
pub fn render_evidence_brief(evidence: &ObservedEvidence) -> String {
    let mut done = Vec::new();
    let mut validation = Vec::new();
    for fact in &evidence.facts {
        match fact.kind {
            ObservedKind::Done => done.push(fact.summary.as_str()),
            ObservedKind::Validation => validation.push(fact.summary.as_str()),
        }
    }

    let mut out =
        String::from("Observed results (cite only these; do not invent others):\n\nDone:\n");
    if done.is_empty() {
        out.push_str("- (no tool-confirmed changes observed this run)\n");
    } else {
        for line in done {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("\nValidation:\n");
    if validation.is_empty() {
        out.push_str("- (no validation commands observed this run)\n");
    } else {
        for line in &validation {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str("\nNot run:\n");
    if validation.is_empty() {
        out.push_str("- No compiler, test, or lint command was observed.\n");
    } else {
        out.push_str("- Any check not listed under Validation was not run.\n");
    }
    out
}

/// Stance appended to local-coder system prompts.
pub const EVIDENCE_FINAL_ANSWER_STANCE: &str = "\
When you finish, use this format:

Done:
- (only changes tools in this session confirmed)

Validation:
- (only commands you ran and their observed outcomes)

Not run:
- (checks you did not run)

You do not know this repository until you inspect it.
Search before making claims about code locations.
Read files before editing them.
Make one focused change at a time.
Do not claim tests passed unless a tool result in this session shows they passed.
Do not claim a file's contents unless you read it in this session.
If you cannot verify a claim, say so rather than inferring.";

fn classify_result(tool_name: &str, text: &str) -> ObservedKind {
    if tool_name == "bash" && looks_like_validation(text) {
        ObservedKind::Validation
    } else {
        ObservedKind::Done
    }
}

fn looks_like_validation(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("cargo test")
        || lower.contains("cargo check")
        || lower.contains("cargo clippy")
        || lower.contains("cargo fmt")
        || lower.contains("pytest")
        || lower.contains("npm test")
        || lower.contains("pnpm test")
        || lower.contains("go test")
        || lower.contains("test result:")
        || lower.contains("running unittests")
}

fn summarize_result(tool_name: &str, is_error: bool, text: &str) -> String {
    let outcome = if is_error { "failed" } else { "ok" };
    let snippet = first_line(text);
    if snippet.is_empty() {
        format!("{tool_name}: {outcome}")
    } else {
        format!("{tool_name}: {outcome} — {snippet}")
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

fn join_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::ToolResultMessage;

    fn tool_result(name: &str, text: &str, is_error: bool) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "c1".into(),
            tool_name: name.into(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            details: serde_json::json!({}),
            is_error,
            timestamp: 1,
        })
    }

    #[test]
    fn evidence_brief_only_cites_observed_results() {
        let evidence = collect_observed_evidence(&[
            tool_result("edit", "updated crates/anie-tui/src/app.rs", false),
            tool_result(
                "bash",
                "$ cargo test -p anie-tui active_input\ntest result: ok. 2 passed",
                false,
            ),
        ]);
        let brief = render_evidence_brief(&evidence);
        assert!(brief.contains("Done:"));
        assert!(brief.contains("edit: ok — updated crates/anie-tui/src/app.rs"));
        assert!(brief.contains("Validation:"));
        assert!(brief.contains("cargo test -p anie-tui active_input"));
        assert!(brief.contains("Not run:"));
        assert!(
            brief.contains("Any check not listed under Validation was not run"),
            "{brief}"
        );
        assert!(
            !brief.to_ascii_lowercase().contains("clippy"),
            "must not invent unobserved clippy: {brief}"
        );
        assert!(
            !brief.contains("workspace"),
            "must not invent a workspace run: {brief}"
        );
    }

    #[test]
    fn evidence_brief_without_observations_does_not_claim_validation() {
        let brief = render_evidence_brief(&ObservedEvidence::default());
        assert!(brief.contains("no tool-confirmed changes observed this run"));
        assert!(brief.contains("no validation commands observed this run"));
        assert!(brief.contains("No compiler, test, or lint command was observed"));
        assert!(!brief.contains("passed"));
    }

    #[test]
    fn read_results_are_done_not_validation() {
        let evidence = collect_observed_evidence(&[tool_result("read", "fn main() {}", false)]);
        assert_eq!(evidence.facts[0].kind, ObservedKind::Done);
        let brief = render_evidence_brief(&evidence);
        assert!(brief.contains("read: ok"));
        assert!(brief.contains("no validation commands observed this run"));
    }
}
