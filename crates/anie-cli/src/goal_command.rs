//! The `/goal` autonomous-loop command: parsing, the armed-goal
//! state, the marker-scanning outcome detector, and the pure
//! continuation decision. The controller drives a fresh run at each
//! clean run-completion boundary; the `self`-bound handlers stay in
//! `controller.rs`.

use anie_protocol::{ContentBlock, Message};

/// An active autonomous goal loop (`/goal`).
pub(crate) struct GoalState {
    pub(crate) goal: String,
    /// Remaining autonomous continuations before the turn cap stops it.
    pub(crate) turns_remaining: u32,
}

/// Runaway guard: a goal self-stops after this many continuations. Each
/// turn is a full agent run, so this is far lower than `/loop`'s cap.
pub(crate) const GOAL_MAX_TURNS: u32 = 50;

/// Sentinels the model emits to end an autonomous goal loop.
pub(crate) const GOAL_COMPLETE_MARKER: &str = "GOAL_COMPLETE";
pub(crate) const GOAL_BLOCKED_MARKER: &str = "GOAL_BLOCKED";

/// Parsed `/goal` argument.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GoalCommand {
    Start(String),
    Stop,
    Status,
}

/// What a just-completed goal turn signalled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GoalOutcome {
    Complete,
    Blocked(String),
}

/// The deferred decision applied after the run-completion drain.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GoalDecision {
    /// Stop the goal with this user-facing message.
    Stop(String),
    /// Keep going (subject to the cap / budget at apply time).
    Continue,
}

/// Parse a `/goal` argument. `None`/empty → `Status`; `stop`/`off`/
/// `cancel` → `Stop`; anything else is the goal description.
pub(crate) fn parse_goal_command(arg: Option<&str>) -> GoalCommand {
    let arg = arg.map(str::trim).filter(|value| !value.is_empty());
    let Some(arg) = arg else {
        return GoalCommand::Status;
    };
    if matches!(arg.to_ascii_lowercase().as_str(), "stop" | "off" | "cancel") {
        return GoalCommand::Stop;
    }
    GoalCommand::Start(arg.to_string())
}

/// Scan a completed run's messages for a goal-completion sentinel.
/// `Complete` wins over `Blocked` if both somehow appear.
pub(crate) fn detect_goal_outcome(messages: &[Message]) -> Option<GoalOutcome> {
    let mut blocked: Option<GoalOutcome> = None;
    for message in messages {
        let Message::Assistant(assistant) = message else {
            continue;
        };
        for block in &assistant.content {
            let ContentBlock::Text { text } = block else {
                continue;
            };
            if text.contains(GOAL_COMPLETE_MARKER) {
                return Some(GoalOutcome::Complete);
            }
            if blocked.is_none()
                && let Some(idx) = text.find(GOAL_BLOCKED_MARKER)
            {
                let reason = text[idx + GOAL_BLOCKED_MARKER.len()..]
                    .trim_start_matches([':', ' '])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                blocked = Some(GoalOutcome::Blocked(reason));
            }
        }
    }
    blocked
}

/// The continuation decision for a goal that is not yet complete: keep
/// going, or stop because the cap or budget says so. Pure for testing.
pub(crate) fn next_goal_step(turns_remaining: u32, budget_blocked: bool) -> GoalDecision {
    if turns_remaining == 0 {
        GoalDecision::Stop(format!(
            "Goal stopped: reached the {GOAL_MAX_TURNS}-turn cap. Run /goal again to keep going."
        ))
    } else if budget_blocked {
        GoalDecision::Stop("Goal stopped: the session budget ceiling was reached.".to_string())
    } else {
        GoalDecision::Continue
    }
}

/// The framing for the first turn of an autonomous goal.
pub(crate) fn initial_goal_prompt(goal: &str) -> String {
    format!(
        "You are working autonomously toward the goal below until it is fully achieved. \
         Plan the steps, execute them with your tools, and VERIFY your work (run tests, \
         re-read files, check outputs). You will be automatically prompted to continue after \
         each turn — keep going without waiting for me.\n\n\
         <goal>\n{goal}\n</goal>\n\n\
         When the goal is fully achieved AND verified, end your message with the exact line:\n\
         {GOAL_COMPLETE_MARKER}\n\n\
         If you are genuinely blocked and need my input to proceed, end your message with:\n\
         {GOAL_BLOCKED_MARKER}: <one-line reason>"
    )
}

/// The framing for each autonomous continuation turn.
pub(crate) fn goal_continuation_prompt(goal: &str) -> String {
    format!(
        "Continue working autonomously toward the goal. Review and verify what you've done so \
         far, then take the next step.\n\n\
         <goal>\n{goal}\n</goal>\n\n\
         End with `{GOAL_COMPLETE_MARKER}` when fully done and verified, or \
         `{GOAL_BLOCKED_MARKER}: <reason>` if you need my input."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{AssistantMessage, StopReason, Usage};

    fn assistant_with_text(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            timestamp: 1,
            reasoning_details: None,
        })
    }

    #[test]
    fn parse_goal_command_handles_start_stop_and_status() {
        assert_eq!(
            parse_goal_command(Some("build a REST API")),
            GoalCommand::Start("build a REST API".into())
        );
        assert_eq!(parse_goal_command(Some("stop")), GoalCommand::Stop);
        assert_eq!(parse_goal_command(Some("CANCEL")), GoalCommand::Stop);
        assert_eq!(parse_goal_command(None), GoalCommand::Status);
        assert_eq!(parse_goal_command(Some("   ")), GoalCommand::Status);
    }

    #[test]
    fn detect_goal_outcome_finds_markers_and_prefers_complete() {
        assert_eq!(
            detect_goal_outcome(&[assistant_with_text("All tests pass.\nGOAL_COMPLETE")]),
            Some(GoalOutcome::Complete)
        );
        assert_eq!(
            detect_goal_outcome(&[assistant_with_text("Stuck.\nGOAL_BLOCKED: need an API key")]),
            Some(GoalOutcome::Blocked("need an API key".into()))
        );
        // Complete wins if both somehow appear.
        assert_eq!(
            detect_goal_outcome(&[
                assistant_with_text("GOAL_BLOCKED: x"),
                assistant_with_text("GOAL_COMPLETE"),
            ]),
            Some(GoalOutcome::Complete)
        );
        assert_eq!(
            detect_goal_outcome(&[assistant_with_text("still working")]),
            None
        );
    }

    #[test]
    fn next_goal_step_caps_turns_and_respects_budget() {
        assert_eq!(next_goal_step(5, false), GoalDecision::Continue);
        assert!(
            matches!(next_goal_step(0, false), GoalDecision::Stop(_)),
            "turn cap"
        );
        assert!(
            matches!(next_goal_step(5, true), GoalDecision::Stop(_)),
            "budget"
        );
    }
}
