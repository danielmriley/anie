use std::{
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use tokio::{io::AsyncReadExt, process::Command, sync::mpsc};
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError};
use anie_protocol::ToolDef;

use crate::shared::{
    IO_DRAIN_TIMEOUT, MAX_READ_BYTES, MAX_READ_LINES, parse_optional_timeout_secs,
    required_string_arg, text_result,
};

/// Execute a shell command in the current working directory.
pub struct BashTool {
    cwd: Arc<PathBuf>,
}

impl BashTool {
    /// Create a bash tool rooted at the provided working directory.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(cwd: P) -> Self {
        Self {
            cwd: Arc::new(cwd.into()),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "bash".into(),
            description: "Execute a bash command in the current working directory. Returns combined stdout and stderr. Supports timeout and cancellation.".into(),
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
        let mut output = self.tail.clone();
        if output.len() > MAX_READ_BYTES {
            let keep_from = output.len() - MAX_READ_BYTES;
            let mut boundary = keep_from;
            while boundary < output.len() && !output.is_char_boundary(boundary) {
                boundary += 1;
            }
            output = output[boundary..].to_string();
        }

        let mut lines: Vec<&str> = output.lines().collect();
        if lines.len() > MAX_READ_LINES {
            lines = lines[lines.len() - MAX_READ_LINES..].to_vec();
        }
        let mut rendered = lines.join("\n");
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
