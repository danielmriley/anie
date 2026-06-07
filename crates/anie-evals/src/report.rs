//! Report rendering: JSON artifact + a Markdown summary with a per-
//! scenario `Δ (rlm vs current)` line.

use crate::{EvalReport, ScenarioReport};

/// Serialize the report to pretty JSON.
///
/// # Errors
/// Returns a `serde_json` error if serialization fails.
pub fn to_json(report: &EvalReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

/// Render a Markdown summary. For each scenario it lists per-mode
/// pass/fail with total tokens and wall-clock, and — when both `current`
/// and `rlm` ran — a delta line (token %, latency %).
#[must_use]
pub fn to_markdown(report: &EvalReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "# Eval report `{}`", report.run_id);
    let _ = writeln!(out, "harness commit: `{}`\n", report.harness_commit);

    for scenario in &report.scenarios {
        let _ = writeln!(out, "## {}", scenario.name);
        let _ = writeln!(out, "| mode | pass | tokens | wall_ms |");
        let _ = writeln!(out, "|---|---|---|---|");
        for result in &scenario.results {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                result.mode,
                if result.pass { "PASS" } else { "FAIL" },
                tokens_of(result),
                wall_ms_of(result),
            );
        }
        if let Some(delta) = delta_line(scenario) {
            let _ = writeln!(out, "\n{delta}");
        }
        let _ = writeln!(out);
    }
    out
}

fn tokens_of(result: &crate::RunResult) -> u64 {
    result.metrics["tokens"]["total_tokens"]
        .as_u64()
        .unwrap_or(0)
}

fn wall_ms_of(result: &crate::RunResult) -> u64 {
    result.metrics["wall_clock_ms"].as_u64().unwrap_or(0)
}

/// `Δ (rlm vs current)` line, when both modes ran for the scenario.
fn delta_line(scenario: &ScenarioReport) -> Option<String> {
    let current = scenario.results.iter().find(|r| r.mode == "current")?;
    let rlm = scenario.results.iter().find(|r| r.mode == "rlm")?;
    let (ct, rt) = (tokens_of(current), tokens_of(rlm));
    let (cw, rw) = (wall_ms_of(current), wall_ms_of(rlm));
    Some(format!(
        "**Δ (rlm vs current):** tokens {}, latency {}",
        pct_delta(ct, rt),
        pct_delta(cw, rw),
    ))
}

fn pct_delta(base: u64, new: u64) -> String {
    if base == 0 {
        return "n/a".to_string();
    }
    let delta = (new as f64 - base as f64) / base as f64 * 100.0;
    format!("{delta:+.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EVAL_REPORT_SCHEMA_VERSION, RunResult};

    fn result(mode: &str, pass: bool, tokens: u64, wall: u64) -> RunResult {
        RunResult {
            mode: mode.into(),
            pass,
            checks: vec![],
            metrics: serde_json::json!({
                "tokens": { "total_tokens": tokens },
                "wall_clock_ms": wall
            }),
        }
    }

    fn report() -> EvalReport {
        EvalReport {
            schema_version: EVAL_REPORT_SCHEMA_VERSION,
            run_id: "2026-06-06T0915Z".into(),
            harness_commit: "abc1234".into(),
            scenarios: vec![ScenarioReport {
                name: "find_thing".into(),
                scenario_sha256: "deadbeef".into(),
                results: vec![
                    result("current", true, 40000, 30000),
                    result("rlm", true, 30000, 41000),
                ],
            }],
        }
    }

    #[test]
    fn report_json_roundtrips_and_includes_scenario_hash() {
        let json = to_json(&report()).expect("json");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(value["schema_version"], EVAL_REPORT_SCHEMA_VERSION);
        assert_eq!(value["scenarios"][0]["scenario_sha256"], "deadbeef");
        assert_eq!(value["scenarios"][0]["results"][1]["mode"], "rlm");
    }

    #[test]
    fn markdown_summary_renders_delta_line_for_two_modes() {
        let md = to_markdown(&report());
        assert!(md.contains("## find_thing"), "{md}");
        // tokens 40000 -> 30000 = -25.0%; latency 30000 -> 41000 = +36.7%.
        assert!(md.contains("Δ (rlm vs current)"), "{md}");
        assert!(md.contains("-25.0%"), "{md}");
        assert!(md.contains("+36.7%"), "{md}");
    }

    #[test]
    fn markdown_omits_delta_when_only_one_mode_ran() {
        let mut r = report();
        r.scenarios[0].results.truncate(1); // only "current"
        let md = to_markdown(&r);
        assert!(!md.contains("Δ"), "{md}");
    }
}
