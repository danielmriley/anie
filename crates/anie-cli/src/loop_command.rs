//! The `/loop` recurring-prompt command: parsing, the armed-loop
//! state, and the `select!` timer helper. The handler methods that
//! mutate the controller live in `controller.rs`; everything here is
//! either pure (parsing/formatting) or self-contained (`wait_until`).

use std::time::Duration;

use tokio::time::{Instant, sleep_until};

/// A scheduled recurring prompt (`/loop <interval> <message>`).
pub(crate) struct LoopState {
    pub(crate) interval: Duration,
    pub(crate) message: String,
    /// When the next fire is due (a `tokio::time::Instant`).
    pub(crate) next_fire: Instant,
    /// Hard safety cap — remaining fires before the loop self-stops. Not
    /// the normal stopping mechanism (`/loop stop` is); guards a runaway.
    pub(crate) fires_remaining: u32,
}

/// Runaway guard: a loop self-stops after this many fires. High enough to
/// be a pure safety net, not a normal stop (use `/loop stop`).
pub(crate) const LOOP_MAX_ITERATIONS: u32 = 1000;

/// Parsed `/loop` argument.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LoopCommand {
    /// Start (or replace) the loop.
    Start { interval: Duration, message: String },
    /// Cancel the active loop.
    Stop,
    /// Report the active loop's status.
    Status,
}

/// Parse a `/loop` argument. `None`/empty → `Status`; `stop`/`off`/
/// `cancel` → `Stop`; otherwise `<Nm|Ns> <message>`. Returns a
/// human-readable error for a malformed interval or empty message.
pub(crate) fn parse_loop_command(arg: Option<&str>) -> Result<LoopCommand, String> {
    let arg = arg.map(str::trim).filter(|value| !value.is_empty());
    let Some(arg) = arg else {
        return Ok(LoopCommand::Status);
    };
    if matches!(arg.to_ascii_lowercase().as_str(), "stop" | "off" | "cancel") {
        return Ok(LoopCommand::Stop);
    }
    let (interval_token, message) = arg
        .split_once(char::is_whitespace)
        .map(|(token, rest)| (token, rest.trim()))
        .unwrap_or((arg, ""));
    let interval = parse_interval(interval_token)?;
    if message.is_empty() {
        return Err(
            "usage: /loop <interval> <message>  (e.g. /loop 3m continue), or /loop stop".into(),
        );
    }
    Ok(LoopCommand::Start {
        interval,
        message: message.to_string(),
    })
}

/// Parse an interval token: `<N>m` (minutes) or `<N>s` (seconds), `N` a
/// positive integer.
fn parse_interval(token: &str) -> Result<Duration, String> {
    let invalid =
        || format!("`{token}` is not a valid interval; use `<N>m` or `<N>s` (e.g. 3m, 30s)");
    let (number, unit) = token.split_at(token.len().saturating_sub(1));
    let secs_per_unit = match unit {
        "m" | "M" => 60,
        "s" | "S" => 1,
        _ => return Err(invalid()),
    };
    let n: u64 = number.parse().map_err(|_| invalid())?;
    if n == 0 {
        return Err("interval must be greater than zero".into());
    }
    Ok(Duration::from_secs(n * secs_per_unit))
}

/// A future that resolves at `deadline`, or never when `None` (so a
/// `select!` arm guarded by it is inert while no loop is armed).
pub(crate) async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Human-readable interval for status/confirmation messages.
pub(crate) fn format_interval(interval: Duration) -> String {
    let secs = interval.as_secs();
    if secs % 60 == 0 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_loop_command_parses_minutes_and_seconds() {
        assert_eq!(
            parse_loop_command(Some("3m continue")),
            Ok(LoopCommand::Start {
                interval: Duration::from_secs(180),
                message: "continue".into()
            })
        );
        assert_eq!(
            parse_loop_command(Some("30s keep going")),
            Ok(LoopCommand::Start {
                interval: Duration::from_secs(30),
                message: "keep going".into()
            })
        );
    }

    #[test]
    fn parse_loop_command_recognizes_stop_aliases_and_status() {
        assert_eq!(parse_loop_command(Some("stop")), Ok(LoopCommand::Stop));
        assert_eq!(parse_loop_command(Some("OFF")), Ok(LoopCommand::Stop));
        assert_eq!(parse_loop_command(Some("cancel")), Ok(LoopCommand::Stop));
        assert_eq!(parse_loop_command(None), Ok(LoopCommand::Status));
        assert_eq!(parse_loop_command(Some("   ")), Ok(LoopCommand::Status));
    }

    #[test]
    fn parse_loop_command_rejects_bad_interval_and_empty_message() {
        assert!(parse_loop_command(Some("5x hello")).is_err(), "bad unit");
        assert!(parse_loop_command(Some("0m hello")).is_err(), "zero");
        assert!(parse_loop_command(Some("abc go")).is_err(), "non-numeric");
        assert!(parse_loop_command(Some("3m")).is_err(), "empty message");
    }

    #[tokio::test]
    async fn wait_until_resolves_at_deadline_and_is_inert_when_none() {
        use tokio::time::{Duration, Instant, timeout};
        // A due (past) deadline resolves promptly — the loop arm fires.
        let due = Instant::now();
        assert!(
            timeout(Duration::from_secs(1), wait_until(Some(due)))
                .await
                .is_ok()
        );
        // `None` never resolves — the arm is inert while no loop is armed.
        assert!(
            timeout(Duration::from_millis(50), wait_until(None))
                .await
                .is_err()
        );
    }
}
