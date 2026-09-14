//! Leftover-gated page-in admit.
//!
//! Virtualization (a lean ceiling + an addressable overflow
//! store) is plumbing. The *admit rule* is the product: a
//! chunk enters the active window only if it residual-reduces
//! the current working sheet, or if it is an orphan failure
//! that nothing in the sheet yet explains.
//!
//! Keyword / embedding similarity against the raw prompt is
//! input-space catchment — it pages in anything that *looks
//! like* the request, including content the working set has
//! already claimed. That path is an A/B hatch
//! (`ANIE_PAGE_IN_ADMIT=keyword`), not the default.
//!
//! The sheet is intentionally small: goal tokens from the
//! latest user directive, plus unexplained validation
//! failures already in the window, minus tokens claimed by
//! successful tool results. Not a TaskState framework.

use std::collections::HashSet;

use anie_protocol::{Message, ToolResultMessage};

use crate::external_context::{first_text, first_text_of, tokenize};

/// How a candidate may enter the active window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitReason {
    /// Candidate tokens hit leftover the active window has
    /// not already claimed.
    ResidualReduce { hits: usize },
    /// Unexplained validation failure: `is_error` and none
    /// of its tokens are claimed by successful evidence.
    Orphan,
}

/// Admit / reject for one overflow candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmitDecision {
    Admit(AdmitReason),
    Reject,
}

impl AdmitDecision {
    /// Sort key: residual-reduce (by hit count) before
    /// orphans. Reject is 0 but never selected.
    pub(crate) fn rank(self) -> u32 {
        match self {
            Self::Admit(AdmitReason::ResidualReduce { hits }) => hits as u32,
            Self::Admit(AdmitReason::Orphan) | Self::Reject => 0,
        }
    }

    pub(crate) fn is_admit(self) -> bool {
        matches!(self, Self::Admit(_))
    }
}

/// Which predicate pages overflow back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PageInAdmit {
    /// Default: residual-reduce or orphan.
    #[default]
    Leftover,
    /// Hatch: keyword overlap (and embedding cosine when an
    /// embedder is wired). `ANIE_PAGE_IN_ADMIT=keyword`.
    Keyword,
}

impl PageInAdmit {
    pub(crate) fn from_env(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("keyword") | Some("embed") | Some("embedding") => Self::Keyword,
            _ => Self::Leftover,
        }
    }
}

/// Residual sensor for one `before_model` fire.
#[derive(Debug, Clone)]
pub(crate) struct WorkingSheet {
    /// Latest user directive, for tests and logs.
    #[allow(dead_code)]
    pub goal: String,
    /// Goal tokens before subtracting claims. Keyword-hatch
    /// scoring still uses this (full prompt, not leftover).
    #[allow(dead_code)]
    pub goal_tokens: HashSet<String>,
    /// Unexplained leftover: goal ∪ in-window failures − claimed.
    pub residual: HashSet<String>,
    /// Tokens already explained by successful tool results.
    pub claimed: HashSet<String>,
}

/// Build a sheet from the post-eviction working set.
/// `None` when there is no real (non-reminder) user text.
pub(crate) fn build_working_sheet(working: &[Message]) -> Option<WorkingSheet> {
    let (goal, goal_ts) = latest_goal(working)?;
    let goal_tokens = tokenize(goal);
    let mut claimed = HashSet::new();
    let mut failure_tokens = HashSet::new();
    for m in working {
        if is_policy_injected(m) {
            continue;
        }
        if matches!(m, Message::User(u) if u.timestamp == goal_ts) {
            continue;
        }
        match m {
            Message::ToolResult(t) if is_validation_failure(t) => {
                failure_tokens.extend(message_tokens(m));
            }
            Message::ToolResult(_) => {
                claimed.extend(message_tokens(m));
            }
            _ => {}
        }
    }
    let mut residual = goal_tokens.clone();
    residual.extend(failure_tokens);
    for tok in &claimed {
        residual.remove(tok);
    }
    Some(WorkingSheet {
        goal: goal.to_string(),
        goal_tokens,
        residual,
        claimed,
    })
}

/// Leftover-gated admit. Keyword overlap with the *full*
/// prompt is not consulted — only residual and orphan.
pub(crate) fn decide_admit(
    sheet: &WorkingSheet,
    candidate_tokens: &HashSet<String>,
    candidate_is_failure: bool,
) -> AdmitDecision {
    let hits = candidate_tokens.intersection(&sheet.residual).count();
    if hits > 0 {
        return AdmitDecision::Admit(AdmitReason::ResidualReduce { hits });
    }
    if candidate_is_failure && candidate_tokens.is_disjoint(&sheet.claimed) {
        return AdmitDecision::Admit(AdmitReason::Orphan);
    }
    AdmitDecision::Reject
}

pub(crate) fn is_validation_failure_message(m: &Message) -> bool {
    match m {
        Message::ToolResult(t) => is_validation_failure(t),
        _ => false,
    }
}

fn is_validation_failure(t: &ToolResultMessage) -> bool {
    t.is_error
}

fn latest_goal(working: &[Message]) -> Option<(&str, u64)> {
    working.iter().rev().find_map(|m| match m {
        Message::User(u) if !is_reminder_user_text(first_text(&u.content)) => {
            first_text(&u.content).map(|text| (text, u.timestamp))
        }
        _ => None,
    })
}

fn is_policy_injected(m: &Message) -> bool {
    first_text_of(m).is_some_and(|t| t.trim_start().starts_with("<system-reminder"))
}

fn is_reminder_user_text(text: Option<&str>) -> bool {
    text.is_some_and(|t| t.trim_start().starts_with("<system-reminder"))
}

fn message_tokens(m: &Message) -> HashSet<String> {
    first_text_of(m).map(tokenize).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{ContentBlock, ToolResultMessage, UserMessage};

    fn user(text: &str, ts: u64) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: ts,
        })
    }

    fn tool_ok(body: &str, ts: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: format!("ok-{ts}"),
            tool_name: "bash".into(),
            content: vec![ContentBlock::Text { text: body.into() }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: ts,
        })
    }

    fn tool_err(body: &str, ts: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: format!("err-{ts}"),
            tool_name: "bash".into(),
            content: vec![ContentBlock::Text { text: body.into() }],
            details: serde_json::Value::Null,
            is_error: true,
            timestamp: ts,
        })
    }

    fn tokens(text: &str) -> HashSet<String> {
        tokenize(text)
    }

    /// Successful tool results claim tokens; the directive
    /// itself does not. Leftover is the goal minus claims.
    #[test]
    fn sheet_subtracts_claimed_evidence_from_goal() {
        let working = vec![
            tool_ok("alphamodule compiles; alphawidget tests passed", 1),
            user(
                "keep going on alphamodule and also fix the flaky betalock test",
                2,
            ),
        ];
        let sheet = build_working_sheet(&working).expect("goal");
        assert!(sheet.claimed.contains("alphamodule"));
        assert!(sheet.claimed.contains("alphawidget"));
        assert!(
            !sheet.residual.contains("alphamodule"),
            "claimed goal token must drop out of leftover: {:?}",
            sheet.residual
        );
        assert!(sheet.residual.contains("betalock"));
        assert!(sheet.residual.contains("flaky"));
    }

    /// Keyword overlap with an already-claimed prompt token
    /// is not leftover-gated admit. This is the falsifier
    /// against input-space catchment.
    #[test]
    fn residual_reduce_rejects_keyword_match_already_claimed() {
        let working = vec![
            tool_ok("alphamodule compiles; alphawidget tests passed", 1),
            user(
                "keep going on alphamodule and also fix the flaky betalock test",
                2,
            ),
        ];
        let sheet = build_working_sheet(&working).expect("goal");
        let keyword_only = tokens("alphamodule notes: alphawidget implementation details");
        assert!(
            !keyword_only.is_disjoint(&sheet.goal_tokens),
            "sanity: keyword hatch would see prompt overlap"
        );
        assert_eq!(
            decide_admit(&sheet, &keyword_only, false),
            AdmitDecision::Reject
        );
    }

    /// A candidate that hits unexplained leftover admits as
    /// residual-reduce, even when it also shares claimed tokens.
    #[test]
    fn residual_reduce_admits_unclaimed_leftover_tokens() {
        let working = vec![
            tool_ok("alphamodule compiles; alphawidget tests passed", 1),
            user(
                "keep going on alphamodule and also fix the flaky betalock test",
                2,
            ),
        ];
        let sheet = build_working_sheet(&working).expect("goal");
        let leftover_hit = tokens("betalock flaky test failed on retry");
        match decide_admit(&sheet, &leftover_hit, false) {
            AdmitDecision::Admit(AdmitReason::ResidualReduce { hits }) => {
                assert!(hits >= 2, "betalock + flaky (and maybe test): {hits}");
            }
            other => panic!("expected residual-reduce, got {other:?}"),
        }
    }

    /// An `is_error` tool result that shares nothing with the
    /// sheet's claimed set is an orphan: nothing explains it
    /// yet, so it may page in even with zero goal overlap.
    #[test]
    fn orphan_failure_admits_when_nothing_explains_it() {
        let working = vec![user("continue the refactor of alphamodule", 1)];
        let sheet = build_working_sheet(&working).expect("goal");
        let orphan = tokens("ld: undefined symbol zetaentry in omegalib");
        assert!(
            orphan.is_disjoint(&sheet.goal_tokens),
            "sanity: not a keyword match on the prompt"
        );
        assert_eq!(
            decide_admit(&sheet, &orphan, true),
            AdmitDecision::Admit(AdmitReason::Orphan)
        );
    }

    /// A failure whose tokens are already claimed by a later
    /// success is not an orphan — the leftover is explained.
    #[test]
    fn claimed_failure_is_not_orphan() {
        let working = vec![
            tool_ok("zetaentry linked; omegalib tests passed", 1),
            user("continue the refactor of alphamodule", 2),
        ];
        let sheet = build_working_sheet(&working).expect("goal");
        let explained = tokens("ld: undefined symbol zetaentry in omegalib");
        assert_eq!(
            decide_admit(&sheet, &explained, true),
            AdmitDecision::Reject
        );
    }

    /// In-window failures add leftover; they do not claim.
    #[test]
    fn in_window_failure_tokens_stay_on_residual() {
        let working = vec![
            tool_err("betalock flaky assertion failed", 1),
            user("make the tests pass", 2),
        ];
        let sheet = build_working_sheet(&working).expect("goal");
        assert!(sheet.residual.contains("betalock"));
        assert!(!sheet.claimed.contains("betalock"));
    }

    /// Policy-injected reminders are not the goal and do not
    /// claim leftover.
    #[test]
    fn reminder_messages_are_not_goal_or_claims() {
        let working = vec![
            user("<system-reminder>archived alphamodule notes</system-reminder>", 1),
            user("fix the flaky betalock test", 2),
        ];
        let sheet = build_working_sheet(&working).expect("goal");
        assert_eq!(sheet.goal, "fix the flaky betalock test");
        assert!(!sheet.claimed.contains("alphamodule"));
        assert!(sheet.residual.contains("betalock"));
    }

    /// Env hatch: only an explicit keyword/embed value leaves
    /// leftover as the default admit.
    #[test]
    fn page_in_admit_default_is_leftover_not_keyword() {
        assert_eq!(PageInAdmit::from_env(None), PageInAdmit::Leftover);
        assert_eq!(PageInAdmit::from_env(Some("")), PageInAdmit::Leftover);
        assert_eq!(PageInAdmit::from_env(Some("leftover")), PageInAdmit::Leftover);
        assert_eq!(PageInAdmit::from_env(Some("keyword")), PageInAdmit::Keyword);
        assert_eq!(PageInAdmit::from_env(Some("embedding")), PageInAdmit::Keyword);
        assert_ne!(PageInAdmit::default(), PageInAdmit::Keyword);
    }

    /// Residual-reduce ranks above orphan so a leftover hit
    /// wins the budget over unexplained noise.
    #[test]
    fn residual_reduce_ranks_above_orphan() {
        let reduce = AdmitDecision::Admit(AdmitReason::ResidualReduce { hits: 1 });
        let orphan = AdmitDecision::Admit(AdmitReason::Orphan);
        assert!(reduce.rank() > orphan.rank());
    }
}
