//! Subprocess runner: invoke the real `anie` binary as a black box,
//! read its `--metrics-out` sidecar, and score the scenario.
//!
//! Subprocess (not library linkage) keeps `anie-evals` a leaf crate and
//! exercises the real CLI surface — `--harness-mode`, `--metrics-out` —
//! exactly as a rival eval treats an agent.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::scenario::{CheckOutcome, CheckResult, Fixture, Scenario, all_passed, evaluate_checks};
use crate::{EvalError, RunMetricsView, RunResult};

/// Default wall-clock cap for a single scenario run. A misbehaving
/// harness mode (network hang, tool loop, deadlock) is killed instead of
/// wedging the whole eval run.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Spawn `command`, drain stdout/stderr on reader threads (so a chatty
/// child can't deadlock on a full pipe), and wait up to `timeout`.
/// Returns `Ok(None)` after killing+reaping the child on timeout.
fn run_with_timeout(mut command: Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    // Put the child in its own process group so a timeout can SIGKILL the
    // whole tree (the child *and* any grandchildren it spawned). Killing
    // only the immediate child orphans grandchildren that keep the stdout
    // pipe open, which would block the drain threads indefinitely.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();

    // Drain pipes concurrently; closing on kill lets these threads finish.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            kill_process_tree(pid, &mut child);
            let _ = out_thread.join();
            let _ = err_thread.join();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok(Some(Output {
        status,
        stdout,
        stderr,
    }))
}

/// SIGKILL the child's whole process group (Unix), then reap the child.
/// Killing the group — not just the child — takes down grandchildren so
/// the inherited stdout/stderr pipes close and the drain threads finish.
fn kill_process_tree(pid: u32, child: &mut std::process::Child) {
    #[cfg(not(unix))]
    let _ = pid; // only used by the group-kill path on unix
    #[cfg(unix)]
    {
        // The child was spawned as its own group leader (pgid == pid),
        // so a negative pid signals the entire group.
        if let Ok(pgid) = i32::try_from(pid) {
            // Safety: kill(2) with a valid pgid and signal; no memory
            // is touched.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Build the `anie` argv for one scenario run under `mode`. Pure (no
/// spawn) so it is unit-tested directly.
#[must_use]
pub fn build_anie_argv(
    mode: &str,
    model: Option<&str>,
    metrics_path: &Path,
    cwd: &Path,
    prompt: &str,
) -> Vec<String> {
    let mut argv = vec![
        "--print".to_string(),
        "--harness-mode".to_string(),
        mode.to_string(),
    ];
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    argv.push("--metrics-out".to_string());
    argv.push(metrics_path.display().to_string());
    argv.push("-C".to_string());
    argv.push(cwd.display().to_string());
    argv.push(prompt.to_string());
    argv
}

/// A prepared fixture sandbox: the temp cwd plus optional cleanup (a git
/// worktree to remove). The `TempDir` cleans itself on drop.
pub struct FixtureSandbox {
    pub cwd: PathBuf,
    _temp: tempfile::TempDir,
    worktree_to_remove: Option<PathBuf>,
}

impl Drop for FixtureSandbox {
    fn drop(&mut self) {
        if let Some(worktree) = &self.worktree_to_remove {
            // Best-effort: detach the worktree so git's metadata doesn't
            // dangle. The temp dir itself is removed by `_temp`.
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(worktree)
                .output();
        }
    }
}

/// Set up a fixture into a fresh temp cwd. `None` → empty dir; `dir` →
/// copy a tree from `repo_root`; `git_ref` → detached worktree at the
/// ref. Both leave the repo working tree untouched.
pub fn setup_fixture(
    fixture: Option<&Fixture>,
    repo_root: &Path,
) -> Result<FixtureSandbox, EvalError> {
    let temp = tempfile::tempdir().map_err(|source| EvalError::Spawn {
        command: "tempdir".to_string(),
        source,
    })?;
    let cwd = temp.path().to_path_buf();
    let mut worktree_to_remove = None;

    match fixture {
        None => {}
        Some(Fixture { dir: Some(dir), .. }) => {
            copy_dir_all(&repo_root.join(dir), &cwd).map_err(|source| EvalError::Spawn {
                command: format!("copy fixture {dir}"),
                source,
            })?;
        }
        Some(Fixture {
            git_ref: Some(git_ref),
            ..
        }) => {
            let output = Command::new("git")
                .args(["worktree", "add", "--detach"])
                .arg(&cwd)
                .arg(git_ref)
                .current_dir(repo_root)
                .output()
                .map_err(|source| EvalError::Spawn {
                    command: "git worktree add".to_string(),
                    source,
                })?;
            if !output.status.success() {
                return Err(EvalError::NonZeroExit {
                    scenario: format!("git worktree add {git_ref}"),
                    status: output.status.to_string(),
                });
            }
            worktree_to_remove = Some(cwd.clone());
        }
        Some(_) => {}
    }

    Ok(FixtureSandbox {
        cwd,
        _temp: temp,
        worktree_to_remove,
    })
}

fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Run one scenario under one harness mode against `anie_bin`, score it,
/// and return the [`RunResult`]. `repo_root` is the base for `dir`/
/// `git_ref` fixtures.
pub fn run_scenario(
    anie_bin: &Path,
    repo_root: &Path,
    scenario: &Scenario,
    mode: &str,
    model: Option<&str>,
    timeout: Duration,
) -> Result<RunResult, EvalError> {
    let sandbox = setup_fixture(scenario.fixture.as_ref(), repo_root)?;
    let metrics_path = sandbox.cwd.join(".anie-metrics.json");
    let argv = build_anie_argv(mode, model, &metrics_path, &sandbox.cwd, &scenario.prompt);

    let mut command = Command::new(anie_bin);
    command.args(&argv);
    let output = run_with_timeout(command, timeout)
        .map_err(|source| EvalError::Spawn {
            command: anie_bin.display().to_string(),
            source,
        })?
        .ok_or_else(|| EvalError::Timeout {
            scenario: scenario.name.clone(),
            secs: timeout.as_secs(),
        })?;
    if !output.status.success() {
        // A crashing mode is a meaningful negative result, not absent
        // data — record it as a FAIL so it appears in the comparison
        // report (with any metrics it managed to emit), rather than
        // returning early and silently dropping the mode.
        let metrics = std::fs::read(&metrics_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .unwrap_or(serde_json::Value::Null);
        return Ok(RunResult {
            mode: mode.to_string(),
            pass: false,
            checks: vec![CheckResult {
                check: "process_exit".into(),
                outcome: CheckOutcome::Fail {
                    reason: format!("anie exited with {}", output.status),
                },
            }],
            metrics,
        });
    }
    let final_text = String::from_utf8_lossy(&output.stdout).into_owned();

    let metrics_bytes = std::fs::read(&metrics_path).map_err(|_| EvalError::MissingMetrics {
        path: metrics_path.display().to_string(),
    })?;
    let metrics_json: serde_json::Value =
        serde_json::from_slice(&metrics_bytes).map_err(|error| EvalError::MetricsParse {
            path: metrics_path.display().to_string(),
            message: error.to_string(),
        })?;
    let metrics: RunMetricsView =
        serde_json::from_value(metrics_json.clone()).map_err(|error| EvalError::MetricsParse {
            path: metrics_path.display().to_string(),
            message: error.to_string(),
        })?;
    // Fail loudly on a metrics schema the runner doesn't understand,
    // rather than silently mis-scoring a changed shape.
    if metrics.schema_version != crate::EXPECTED_RUN_METRICS_SCHEMA_VERSION {
        return Err(EvalError::MetricsSchemaMismatch {
            path: metrics_path.display().to_string(),
            found: metrics.schema_version,
            expected: crate::EXPECTED_RUN_METRICS_SCHEMA_VERSION,
        });
    }

    let checks = evaluate_checks(&scenario.expect, &final_text, &metrics);
    Ok(RunResult {
        mode: mode.to_string(),
        pass: all_passed(&checks),
        checks,
        metrics: metrics_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Fixture;

    #[test]
    fn runner_builds_expected_anie_argv_for_mode_and_metrics_out() {
        let argv = build_anie_argv(
            "rlm",
            Some("gpt-4o"),
            Path::new("/tmp/m.json"),
            Path::new("/tmp/cwd"),
            "do the thing",
        );
        assert_eq!(
            argv,
            vec![
                "--print",
                "--harness-mode",
                "rlm",
                "--model",
                "gpt-4o",
                "--metrics-out",
                "/tmp/m.json",
                "-C",
                "/tmp/cwd",
                "do the thing",
            ]
        );
    }

    #[test]
    fn build_anie_argv_omits_model_when_absent() {
        let argv = build_anie_argv("current", None, Path::new("/m"), Path::new("/c"), "p");
        assert!(!argv.iter().any(|a| a == "--model"));
    }

    #[test]
    fn fixture_dir_is_copied_into_temp_cwd_and_cleaned_up() {
        let repo = tempfile::tempdir().expect("repo");
        let fixture_dir = repo.path().join("fix");
        std::fs::create_dir_all(fixture_dir.join("sub")).expect("mkdir");
        std::fs::write(fixture_dir.join("a.txt"), "hello").expect("write");
        std::fs::write(fixture_dir.join("sub/b.txt"), "world").expect("write");

        let cwd_path;
        {
            let sandbox = setup_fixture(
                Some(&Fixture {
                    dir: Some("fix".to_string()),
                    git_ref: None,
                }),
                repo.path(),
            )
            .expect("setup");
            cwd_path = sandbox.cwd.clone();
            assert_eq!(
                std::fs::read_to_string(sandbox.cwd.join("a.txt")).unwrap(),
                "hello"
            );
            assert_eq!(
                std::fs::read_to_string(sandbox.cwd.join("sub/b.txt")).unwrap(),
                "world"
            );
        }
        // TempDir cleaned up on drop.
        assert!(!cwd_path.exists());
    }

    #[test]
    fn no_fixture_yields_empty_temp_cwd() {
        let repo = tempfile::tempdir().expect("repo");
        let sandbox = setup_fixture(None, repo.path()).expect("setup");
        assert!(sandbox.cwd.is_dir());
        assert_eq!(std::fs::read_dir(&sandbox.cwd).unwrap().count(), 0);
    }
}
