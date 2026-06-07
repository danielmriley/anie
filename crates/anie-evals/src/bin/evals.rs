//! `evals` — run anie scenarios under one or more harness modes and emit
//! a JSON + Markdown comparison report. See `docs/eval_harness/`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use clap::Parser;

use anie_evals::{
    DEFAULT_RUN_TIMEOUT, EVAL_REPORT_SCHEMA_VERSION, EvalReport, ScenarioReport, load_scenario,
    run_scenario, scenario_sha256, to_json, to_markdown,
};

#[derive(Debug, Parser)]
#[command(name = "evals", about = "Run anie eval scenarios and report")]
struct Args {
    /// Scenario TOML files to run.
    #[arg(long, required = true, num_args = 1..)]
    scenarios: Vec<PathBuf>,
    /// Harness modes to compare (comma-separated): baseline,current,rlm.
    #[arg(long, default_value = "current")]
    modes: String,
    /// Model id passed to anie (omit to use anie's configured default).
    #[arg(long)]
    model: Option<String>,
    /// Output path stem; writes `<out>.json` and `<out>.md`.
    #[arg(long, default_value = "eval-report")]
    out: PathBuf,
    /// Path to the `anie` binary (default: search target/release then PATH).
    #[arg(long)]
    anie_bin: Option<PathBuf>,
    /// Repo root for `dir`/`git_ref` fixtures (default: cwd).
    #[arg(long)]
    repo_root: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let anie_bin = resolve_anie_bin(args.anie_bin);
    let repo_root = args
        .repo_root
        .unwrap_or(std::env::current_dir().context("cwd")?);
    let modes: Vec<&str> = args.modes.split(',').map(str::trim).collect();

    let mut scenarios = Vec::new();
    for path in &args.scenarios {
        let scenario =
            load_scenario(path).with_context(|| format!("loading {}", path.display()))?;
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let sha = scenario_sha256(&bytes);

        let mut results = Vec::new();
        for mode in &modes {
            eprintln!("running `{}` under `{mode}`…", scenario.name);
            match run_scenario(
                &anie_bin,
                &repo_root,
                &scenario,
                mode,
                args.model.as_deref(),
                DEFAULT_RUN_TIMEOUT,
            ) {
                Ok(result) => {
                    eprintln!("  {}: {}", mode, if result.pass { "PASS" } else { "FAIL" });
                    results.push(result);
                }
                Err(error) => eprintln!("  {mode}: ERROR {error}"),
            }
        }
        scenarios.push(ScenarioReport {
            name: scenario.name.clone(),
            scenario_sha256: sha,
            results,
        });
    }

    let report = EvalReport {
        schema_version: EVAL_REPORT_SCHEMA_VERSION,
        run_id: now_stamp(),
        harness_commit: git_short_head(),
        scenarios,
    };

    let json_path = args.out.with_extension("json");
    let md_path = args.out.with_extension("md");
    std::fs::write(&json_path, to_json(&report).context("serialize report")?)
        .with_context(|| format!("writing {}", json_path.display()))?;
    std::fs::write(&md_path, to_markdown(&report))
        .with_context(|| format!("writing {}", md_path.display()))?;
    eprintln!("wrote {} and {}", json_path.display(), md_path.display());
    Ok(())
}

fn resolve_anie_bin(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    let release = PathBuf::from("target/release/anie");
    if release.is_file() {
        return release;
    }
    PathBuf::from("anie") // fall back to PATH
}

fn git_short_head() -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn now_stamp() -> String {
    // Coarse run id; the report is keyed by harness_commit + this stamp.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("run-{secs}")
}
