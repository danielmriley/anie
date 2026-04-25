use std::{
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use regex::Regex;
use tokio::{io::AsyncReadExt, process::Command, sync::mpsc};
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::shared::{
    IO_DRAIN_TIMEOUT, MAX_READ_BYTES, MAX_READ_LINES, parse_optional_timeout_secs,
    required_string_arg, text_result,
};

/// Execute a shell command with the session cwd as the starting
/// directory. The command is not sandboxed.
pub struct BashTool {
    cwd: Arc<PathBuf>,
    policy: BashPolicy,
}

/// Pre-spawn bash command deny policy.
///
/// This is an accidental-risk guardrail, not a sandbox. It checks the
/// raw command string and simple command words before any process is
/// spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashPolicy {
    /// Whether the policy is active.
    pub enabled: bool,
    /// Exact command names to deny. Basenames are matched, so
    /// `/bin/rm` matches `rm`.
    pub deny_commands: Vec<String>,
    /// Regex patterns matched against the raw command string.
    pub deny_patterns: Vec<String>,
}

impl Default for BashPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            deny_commands: Vec::new(),
            deny_patterns: Vec::new(),
        }
    }
}

impl BashTool {
    /// Create a bash tool using the provided working directory as the
    /// process start directory.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self::with_policy(cwd, BashPolicy::default())
    }

    /// Create a bash tool using a pre-spawn deny policy.
    #[must_use]
    pub fn with_policy<P: Into<PathBuf>>(cwd: P, policy: BashPolicy) -> Self {
        Self {
            cwd: Arc::new(cwd.into()),
            policy,
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash".into(),
            description: "Execute a bash command with the session cwd as the starting directory. The command is not sandboxed and has the same system access as the anie process. Returns combined stdout and stderr. Supports timeout and cancellation.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" },
                    "timeout": { "type": "number", "description": "Timeout in seconds (optional)" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        update_tx: Option<mpsc::Sender<anie_protocol::ToolResult>>,
    ) -> Result<anie_protocol::ToolResult, ToolError> {
        let command = required_string_arg(&args, "command")?;
        self.policy.check(command)?;
        let timeout = parse_optional_timeout_secs(&args)?;
        let started = Instant::now();
        let (shell, shell_args) = shell_command(command);
        let mut child_command = Command::new(shell);
        child_command
            .args(shell_args)
            .current_dir(self.cwd.as_ref())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        child_command.process_group(0);

        let mut child = child_command.spawn().map_err(|error| {
            ToolError::ExecutionFailed(format!("Failed to spawn shell command: {error}"))
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (chunk_tx, mut chunk_rx) = mpsc::channel::<String>(32);

        if let Some(stdout) = stdout {
            let stdout_tx = chunk_tx.clone();
            tokio::spawn(async move {
                let _ = forward_pipe(stdout, stdout_tx).await;
            });
        }
        if let Some(stderr) = stderr {
            let stderr_tx = chunk_tx.clone();
            tokio::spawn(async move {
                let _ = forward_pipe(stderr, stderr_tx).await;
            });
        }
        drop(chunk_tx);

        let mut collector = OutputCollector::new();
        let mut timeout_sleep =
            timeout.map(|spec| Box::pin(tokio::time::sleep(spec.as_duration())));
        let mut exit_status = None;
        let mut timed_out = false;
        let mut aborted = false;

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    aborted = true;
                    kill_process_tree(child.id());
                    break;
                }
                _ = async {
                    if let Some(timeout_sleep) = &mut timeout_sleep {
                        timeout_sleep.as_mut().await;
                    }
                }, if timeout_sleep.is_some() => {
                    timed_out = true;
                    kill_process_tree(child.id());
                    break;
                }
                Some(chunk) = chunk_rx.recv() => {
                    collector.push(&chunk);
                    if let Some(update_tx) = &update_tx {
                        let _ = update_tx.send(text_result(collector.render(), serde_json::json!({"partial": true}))).await;
                    }
                }
                status = child.wait(), if exit_status.is_none() => {
                    exit_status = Some(status.map_err(|error| {
                        ToolError::ExecutionFailed(format!("Failed to wait for command: {error}"))
                    })?);
                    break;
                }
            }
        }

        let drain_deadline = tokio::time::Instant::now() + IO_DRAIN_TIMEOUT;
        while tokio::time::Instant::now() < drain_deadline {
            match tokio::time::timeout(Duration::from_millis(50), chunk_rx.recv()).await {
                Ok(Some(chunk)) => collector.push(&chunk),
                Ok(None) | Err(_) => break,
            }
        }

        let output = collector.render();
        if aborted {
            return Err(ToolError::Aborted);
        }
        if timed_out {
            return Err(ToolError::Timeout(
                timeout.map(|spec| spec.whole_seconds()).unwrap_or(0),
            ));
        }

        let exit_status = exit_status
            .ok_or_else(|| ToolError::ExecutionFailed("Command exited without a status".into()))?;
        let exit_code = exit_status.code().unwrap_or_default();

        if !exit_status.success() {
            let message = if output.is_empty() {
                format!("Command exited with status {exit_code}")
            } else {
                format!("Command exited with status {exit_code}\n{output}")
            };
            return Err(ToolError::ExecutionFailed(message));
        }

        let display = if output.is_empty() {
            "[command completed successfully with no output]".into()
        } else {
            output
        };
        Ok(text_result(
            display,
            serde_json::json!({
                "command": command,
                "exit_code": exit_code,
                "truncated": collector.was_truncated(),
                "elapsed_ms": started.elapsed().as_millis(),
            }),
        ))
    }
}

impl BashPolicy {
    fn check(&self, command: &str) -> Result<(), ToolError> {
        if !self.enabled {
            return Ok(());
        }

        for command_name in command_names(command) {
            if self
                .deny_commands
                .iter()
                .any(|denied| denied == &command_name)
            {
                return Err(ToolError::ExecutionFailed(format!(
                    "blocked by bash policy: command '{command_name}' is denied"
                )));
            }
        }

        for pattern in &self.deny_patterns {
            let regex = Regex::new(pattern).map_err(|error| {
                ToolError::ExecutionFailed(format!(
                    "invalid bash policy deny pattern '{pattern}': {error}"
                ))
            })?;
            if regex.is_match(command) {
                return Err(ToolError::ExecutionFailed(format!(
                    "blocked by bash policy: matched deny pattern '{pattern}'"
                )));
            }
        }

        Ok(())
    }
}

fn command_names(command: &str) -> Vec<String> {
    command
        .split([';', '|', '&', '\n'])
        .filter_map(first_command_name)
        .collect()
}

fn first_command_name(segment: &str) -> Option<String> {
    let mut tokens = segment.split_whitespace();
    loop {
        let token = tokens.next()?;
        let token = token.trim_matches(|ch| matches!(ch, '(' | ')' | '{' | '}'));
        if token.is_empty() || token.contains('=') {
            continue;
        }
        let basename = token
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(token)
            .trim_matches(|ch| matches!(ch, '"' | '\''));
        if matches!(basename, "sudo" | "command" | "env" | "nohup" | "time") {
            continue;
        }
        return Some(basename.to_string());
    }
}

async fn forward_pipe<R>(mut reader: R, sender: mpsc::Sender<String>) -> std::io::Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut buffer = [0u8; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
        if sender.send(chunk).await.is_err() {
            break;
        }
    }
    Ok(())
}

fn shell_command(command: &str) -> (String, Vec<String>) {
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                which::which("bash")
                    .ok()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| "/bin/sh".into());
        (shell, vec!["-lc".into(), command.into()])
    }

    #[cfg(windows)]
    {
        if let Ok(path) = which::which("pwsh") {
            return (
                path.display().to_string(),
                vec!["-NoProfile".into(), "-Command".into(), command.into()],
            );
        }
        if let Ok(path) = which::which("powershell") {
            return (
                path.display().to_string(),
                vec!["-NoProfile".into(), "-Command".into(), command.into()],
            );
        }
        (
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
            vec!["/C".into(), command.into()],
        )
    }
}

fn kill_process_tree(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };

    #[cfg(unix)]
    {
        let process_group = nix::unistd::Pid::from_raw(-(pid as i32));
        let _ = nix::sys::signal::kill(process_group, nix::sys::signal::Signal::SIGKILL);
    }

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
}

struct OutputCollector {
    total_bytes: usize,
    total_lines: usize,
    tail: String,
    truncated: bool,
}

impl OutputCollector {
    fn new() -> Self {
        Self {
            total_bytes: 0,
            total_lines: 0,
            tail: String::new(),
            truncated: false,
        }
    }

    fn push(&mut self, chunk: &str) {
        self.total_bytes += chunk.len();
        self.total_lines += chunk.lines().count();
        self.tail.push_str(chunk);
        if self.tail.len() > MAX_READ_BYTES * 4 {
            let keep_from = self.tail.len().saturating_sub(MAX_READ_BYTES * 2);
            let mut boundary = keep_from;
            while boundary < self.tail.len() && !self.tail.is_char_boundary(boundary) {
                boundary += 1;
            }
            self.tail = self.tail[boundary..].to_string();
            self.truncated = true;
        }
    }

    fn render(&self) -> String {
        // Plan 07 PR-C: slice directly from `self.tail`
        // without cloning the full tail string first. The
        // final `lines[start..].join("\n")` still allocates
        // the output, but we save the intermediate tail-clone
        // which was up to 2×MAX_READ_BYTES (100 KB) on busy
        // shells.
        let tail_slice: &str = if self.tail.len() > MAX_READ_BYTES {
            let keep_from = self.tail.len() - MAX_READ_BYTES;
            let mut boundary = keep_from;
            while boundary < self.tail.len() && !self.tail.is_char_boundary(boundary) {
                boundary += 1;
            }
            &self.tail[boundary..]
        } else {
            &self.tail
        };

        let lines: Vec<&str> = tail_slice.lines().collect();
        let start = lines.len().saturating_sub(MAX_READ_LINES);
        let mut rendered = lines[start..].join("\n");
        if self.was_truncated() {
            if !rendered.is_empty() {
                rendered.push('\n');
            }
            rendered.push_str("[output truncated]");
        }
        rendered
    }

    fn was_truncated(&self) -> bool {
        self.truncated || self.total_bytes > MAX_READ_BYTES || self.total_lines > MAX_READ_LINES
    }
}
