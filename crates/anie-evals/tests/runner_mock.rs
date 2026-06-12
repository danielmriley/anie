//! CI-safe golden test: drive `run_scenario` against a *fake* anie binary
//! (a shell script that writes a metrics JSON and prints final text), so
//! the runner is exercised end-to-end without a model or the real anie.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use anie_evals::{parse_scenario, run_scenario};

/// Write an executable fake-anie script. It scans argv for
/// `--metrics-out` and `--harness-mode`, writes a metrics JSON whose
/// token total varies by mode (so multi-mode deltas are visible), and
/// prints a fixed final line to stdout.
fn write_fake_anie(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-anie.sh");
    let script = r#"#!/usr/bin/env bash
metrics=""; mode="current"
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[i]}" in
    --metrics-out) metrics="${args[i+1]}" ;;
    --harness-mode) mode="${args[i+1]}" ;;
  esac
done
if [ "$mode" = "rlm" ]; then tot=30000; else tot=40000; fi
cat > "$metrics" <<EOF
{"schema_version":5,"harness_mode":"$mode","model":"mock","provider":"mock","wall_clock_ms":120,"turns":1,"tokens":{"input_tokens":$tot,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"total_tokens":$tot},"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0},"tools":{"calls":2,"failures":0,"by_tool":{"grep":{"calls":2,"failures":0}}},"compaction":{"pre_prompt":0,"mid_turn":0,"reactive_overflow":0,"total":0},"recovery":{"verify_runs":0,"verify_failures":0,"loop_perturbations":0,"grounded_edit_failures":0},"prompt":{"system_prompt_tokens":0},"context":{"evictions":0,"evicted_tokens":0,"page_ins":0,"page_in_tokens":0,"ledger_tokens_total":0,"prefill_tokens_total":0,"truncation_suspected":0}}
EOF
echo "Found CompactionStatsAtomic in compaction_stats.rs"
"#;
    std::fs::write(&path, script).expect("write fake");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

const PASSING_SCENARIO: &str = r#"
name = "find_compaction_stats"
family = "repo_navigation"
prompt = "Which struct tracks per-phase compaction counts?"
[expect]
contains = ["CompactionStatsAtomic"]
must_call_tool = "grep"
min_tool_calls = 1
max_tokens = 60000
"#;

#[test]
fn mock_run_produces_pass_result_and_populated_metrics() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = write_fake_anie(dir.path());
    let scenario = parse_scenario(PASSING_SCENARIO).expect("scenario");

    let result = run_scenario(
        &fake,
        dir.path(),
        &scenario,
        "current",
        Some("mock"),
        anie_evals::DEFAULT_RUN_TIMEOUT,
    )
    .expect("run");
    assert!(result.pass, "checks: {:?}", result.checks);
    assert_eq!(result.metrics["tools"]["calls"], 2);
    assert_eq!(result.metrics["tokens"]["total_tokens"], 40000);
}

#[test]
fn baseline_current_rlm_compared_on_one_scenario_yields_three_results() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = write_fake_anie(dir.path());
    let scenario = parse_scenario(PASSING_SCENARIO).expect("scenario");

    let results: Vec<_> = ["baseline", "current", "rlm"]
        .iter()
        .map(|mode| {
            run_scenario(
                &fake,
                dir.path(),
                &scenario,
                mode,
                Some("mock"),
                anie_evals::DEFAULT_RUN_TIMEOUT,
            )
            .expect("run")
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|r| r.pass));
    // rlm reports fewer tokens than current (the fake's mode-varying total).
    let tokens = |mode: &str| {
        results.iter().find(|r| r.mode == mode).unwrap().metrics["tokens"]["total_tokens"]
            .as_u64()
            .unwrap()
    };
    assert!(tokens("rlm") < tokens("current"));
}

#[test]
fn failing_assertion_marks_run_failed_with_reason() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = write_fake_anie(dir.path());
    // Require a string the fake never prints.
    let scenario = parse_scenario(
        "name = \"x\"\nfamily = \"f\"\nprompt = \"p\"\n[expect]\ncontains = [\"NEVER_PRINTED\"]\n",
    )
    .expect("scenario");

    let result = run_scenario(
        &fake,
        dir.path(),
        &scenario,
        "current",
        Some("mock"),
        anie_evals::DEFAULT_RUN_TIMEOUT,
    )
    .expect("run");
    assert!(!result.pass);
    assert!(
        result
            .checks
            .iter()
            .any(|c| matches!(&c.outcome, anie_evals::CheckOutcome::Fail { reason } if reason.contains("NEVER_PRINTED"))),
        "checks: {:?}",
        result.checks
    );
}

/// A fake anie that hangs forever — the runner must kill it on timeout
/// rather than blocking the whole eval run.
fn write_hanging_anie(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("hang-anie.sh");
    std::fs::write(&path, "#!/usr/bin/env bash\nsleep 600\n").expect("write hang");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

#[test]
fn hung_anie_is_killed_and_reported_as_timeout() {
    use anie_evals::EvalError;
    let dir = tempfile::tempdir().expect("dir");
    let fake = write_hanging_anie(dir.path());
    let scenario = parse_scenario(PASSING_SCENARIO).expect("scenario");

    let start = std::time::Instant::now();
    let err = run_scenario(
        &fake,
        dir.path(),
        &scenario,
        "current",
        Some("mock"),
        std::time::Duration::from_millis(300),
    )
    .expect_err("a hung child must time out");
    assert!(matches!(err, EvalError::Timeout { .. }), "{err:?}");
    // The killed child returns promptly, well under the 600s it would sleep.
    assert!(start.elapsed() < std::time::Duration::from_secs(30));
}

/// A fake anie that writes valid metrics but exits non-zero.
fn write_crashing_anie(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("crash-anie.sh");
    let script = r#"#!/usr/bin/env bash
metrics=""
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[i]}" in --metrics-out) metrics="${args[i+1]}" ;; esac
done
cat > "$metrics" <<EOM
{"schema_version":5,"harness_mode":"current","model":"mock","provider":"mock","wall_clock_ms":1,"turns":1,"tokens":{"input_tokens":1,"output_tokens":0,"cache_read_tokens":0,"cache_write_tokens":0,"total_tokens":1},"cost":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write":0.0,"total":0.0},"tools":{"calls":0,"failures":0,"by_tool":{}},"compaction":{"pre_prompt":0,"mid_turn":0,"reactive_overflow":0,"total":0}}
EOM
exit 3
"#;
    std::fs::write(&path, script).expect("write crash");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

#[test]
fn non_zero_exit_records_a_failed_result_not_a_dropped_mode() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = write_crashing_anie(dir.path());
    let scenario = parse_scenario(PASSING_SCENARIO).expect("scenario");

    let result = run_scenario(
        &fake,
        dir.path(),
        &scenario,
        "current",
        Some("mock"),
        anie_evals::DEFAULT_RUN_TIMEOUT,
    )
    .expect("a crash is recorded, not returned as Err");
    assert!(!result.pass, "a non-zero exit is a FAIL");
    assert!(result.checks.iter().any(|c| c.check == "process_exit"));
}

/// A fake anie whose metrics declare an unsupported schema version.
fn write_future_schema_anie(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("future-anie.sh");
    let script = r#"#!/usr/bin/env bash
metrics=""
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  case "${args[i]}" in --metrics-out) metrics="${args[i+1]}" ;; esac
done
cat > "$metrics" <<EOM
{"schema_version":99,"harness_mode":"current","model":"mock","provider":"mock","wall_clock_ms":1,"turns":1,"tokens":{"total_tokens":1},"tools":{"calls":0,"failures":0,"by_tool":{}}}
EOM
echo done
"#;
    std::fs::write(&path, script).expect("write future");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("meta").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

#[test]
fn unsupported_metrics_schema_version_fails_loudly() {
    use anie_evals::EvalError;
    let dir = tempfile::tempdir().expect("dir");
    let fake = write_future_schema_anie(dir.path());
    let scenario = parse_scenario(PASSING_SCENARIO).expect("scenario");

    let err = run_scenario(
        &fake,
        dir.path(),
        &scenario,
        "current",
        Some("mock"),
        anie_evals::DEFAULT_RUN_TIMEOUT,
    )
    .expect_err("a future schema must not be silently scored");
    assert!(
        matches!(err, EvalError::MetricsSchemaMismatch { found: 99, .. }),
        "{err:?}"
    );
}
