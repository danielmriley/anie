//! Minimal, non-interactive evaluation harness for anie.
//!
//! A leaf crate (no `anie-cli` dependency): it runs the real `anie`
//! binary as a black-box subprocess (see `runner`, PR3), reads the
//! `RunMetrics` JSON sidecar it emits via `--metrics-out`, and scores
//! TOML scenarios with deterministic automated checks. See
//! `docs/eval_harness/README.md`.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod report;
mod runner;
mod scenario;

pub use report::{to_json, to_markdown};
pub use runner::{FixtureSandbox, build_anie_argv, run_scenario, setup_fixture};
pub use scenario::{
    CheckOutcome, CheckResult, Expect, Fixture, Scenario, all_passed, evaluate_checks,
    load_scenario, parse_scenario, scenario_sha256,
};

use serde::{Deserialize, Serialize};

/// Report schema version (sidecar artifact; independent of the session
/// schema).
pub const EVAL_REPORT_SCHEMA_VERSION: u32 = 1;

/// Typed errors for the eval harness — no string-matching recovery.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("failed to read scenario `{path}`: {source}")]
    ScenarioRead {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid scenario `{path}`: {message}")]
    ScenarioParse { path: String, message: String },
    #[error("eval runner failed to spawn `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("anie exited with status {status} for scenario `{scenario}`")]
    NonZeroExit { scenario: String, status: String },
    #[error("metrics file `{path}` was not produced")]
    MissingMetrics { path: String },
    #[error("could not parse metrics `{path}`: {message}")]
    MetricsParse { path: String, message: String },
}

/// A deserialization view of anie-cli's `RunMetrics` JSON. anie-specific
/// (deviation): this mirrors `anie-cli/src/run_metrics.rs` rather than
/// importing it, so `anie-evals` stays a leaf crate that talks to anie
/// only across the `--metrics-out` JSON boundary. `schema_version` lets
/// the runner detect drift; serde ignores any fields not modelled here.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RunMetricsView {
    pub schema_version: u32,
    #[serde(default)]
    pub harness_mode: String,
    #[serde(default)]
    pub wall_clock_ms: u64,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub tokens: TokenView,
    #[serde(default)]
    pub tools: ToolView,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct TokenView {
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ToolView {
    #[serde(default)]
    pub calls: u32,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub by_tool: std::collections::BTreeMap<String, ToolOutcomeView>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ToolOutcomeView {
    #[serde(default)]
    pub calls: u32,
    #[serde(default)]
    pub failures: u32,
}

/// Result of running one scenario under one harness mode.
#[derive(Debug, Clone, Serialize)]
pub struct RunResult {
    pub mode: String,
    pub pass: bool,
    pub checks: Vec<CheckResult>,
    /// The raw metrics JSON for the run (reproduced verbatim in the
    /// report).
    pub metrics: serde_json::Value,
}

/// Aggregate report across scenarios and modes.
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub schema_version: u32,
    pub run_id: String,
    pub harness_commit: String,
    pub scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub name: String,
    pub scenario_sha256: String,
    pub results: Vec<RunResult>,
}
