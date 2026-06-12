//! Harness-run verification (plan 03 §2a of
//! `docs/local_model_augmentation/`).
//!
//! A [`BeforeModelPolicy`] that runs the user-configured `[verify]`
//! command after a turn in which a mutating tool (`edit` / `write` /
//! `apply_patch`) succeeded, and injects the result as ONE
//! **context-only** `<system-reminder source="verify">` message —
//! present in the request, never persisted to the session (the
//! `VerifierPolicy` precedent). The harness is the caller, not the
//! model: this deliberately does not go through the bash tool's
//! `Tool` interface, so the run never looks like a model action in
//! the transcript — but it mirrors `bash.rs`'s process hygiene (own
//! process group, `kill_on_drop`, SIGKILL on timeout).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse};
use anie_protocol::{AgentEvent, ContentBlock, Message, UserMessage, now_millis};

/// Successful results from these tools trigger a verify run.
const MUTATING_TOOLS: [&str; 3] = ["edit", "write", "apply_patch"];

/// Stable prefix on every verify `SystemMessage` event. `run_metrics`
/// counts `recovery.verify_runs` off this prefix — keep them in sync.
pub(crate) const VERIFY_EVENT_PREFIX: &str = "[verify]";

/// Stable prefix on failed-verify `SystemMessage` events (exit != 0,
/// timeout, or spawn failure). `run_metrics` counts
/// `recovery.verify_failures` off this prefix.
pub(crate) const VERIFY_FAILURE_EVENT_PREFIX: &str = "[verify] FAILED";

/// Cap on bytes retained per stream while draining the child. The
/// reminder only ever shows the first `max_output_lines` lines, so
/// anything past this is drained (to keep the pipe from blocking)
/// but not stored.
const STREAM_RETAIN_BYTES: usize = 64 * 1024;

/// Outcome of one verify-command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerifyOutcome {
    Passed,
    Failed {
        /// Short cause, e.g. `exit 101` or `timeout after 120s`.
        cause: String,
        /// Combined stderr-then-stdout text (stderr first — compile
        /// errors front-load there). Uncapped; the reminder renderer
        /// applies the line cap.
        output: String,
    },
}

/// Spawns the configured verify command with bash-tool process
/// hygiene and classifies the result.
pub(crate) struct VerifyRunner {
    command: String,
    timeout: Duration,
    max_output_lines: usize,
    cwd: PathBuf,
    sandbox: Option<anie_sandbox::SandboxSpec>,
}

impl VerifyRunner {
    pub(crate) fn new(
        command: String,
        timeout: Duration,
        max_output_lines: usize,
        cwd: PathBuf,
        sandbox: Option<anie_sandbox::SandboxSpec>,
    ) -> Self {
        Self {
            command,
            timeout,
            max_output_lines,
            cwd,
            sandbox,
        }
    }

    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    pub(crate) async fn run(&self) -> VerifyOutcome {
        let (shell, shell_args) = shell_command(&self.command);
        let mut child_command = tokio::process::Command::new(shell);
        child_command
            .args(shell_args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        child_command.process_group(0);
        // Same trust level as the model-invoked bash tool: unsandboxed
        // by default, confined when `[tools.sandbox]` is configured.
        if let Err(error) = anie_sandbox::apply(&mut child_command, self.sandbox.as_ref()) {
            return VerifyOutcome::Failed {
                cause: "sandbox setup failed".to_string(),
                output: error.to_string(),
            };
        }
        let mut child = match child_command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return VerifyOutcome::Failed {
                    cause: "spawn failed".to_string(),
                    output: error.to_string(),
                };
            }
        };
        let pid = child.id();
        let stdout_task = child.stdout.take().map(|pipe| {
            tokio::spawn(async move { drain_retaining_head(pipe, STREAM_RETAIN_BYTES).await })
        });
        let stderr_task = child.stderr.take().map(|pipe| {
            tokio::spawn(async move { drain_retaining_head(pipe, STREAM_RETAIN_BYTES).await })
        });

        let mut timed_out = false;
        let status = tokio::select! {
            status = child.wait() => status.ok(),
            () = tokio::time::sleep(self.timeout) => {
                timed_out = true;
                kill_process_group(pid).await;
                let _ = child.wait().await;
                None
            }
        };

        let mut output = String::new();
        // stderr first: compile errors front-load there (plan 03 §2a).
        for task in [stderr_task, stdout_task].into_iter().flatten() {
            if let Ok(bytes) = task.await {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
        }

        if timed_out {
            return VerifyOutcome::Failed {
                cause: format!("timeout after {}s; process group killed", self.timeout.as_secs()),
                output,
            };
        }
        match status {
            Some(status) if status.success() => VerifyOutcome::Passed,
            Some(status) => VerifyOutcome::Failed {
                cause: match status.code() {
                    Some(code) => format!("exit {code}"),
                    None => "killed by signal".to_string(),
                },
                output,
            },
            None => VerifyOutcome::Failed {
                cause: "wait failed".to_string(),
                output,
            },
        }
    }
}

/// Build the `[tools.sandbox]` spec for the verify child. Mirrors the
/// private `sandbox_spec_from_config` in `bootstrap.rs` (empty
/// `writable_roots` defaults to `[cwd, $TMPDIR]`).
pub(crate) fn sandbox_spec(
    sandbox: &anie_config::SandboxToolConfig,
    cwd: &Path,
) -> Option<anie_sandbox::SandboxSpec> {
    if !sandbox.enabled {
        return None;
    }
    let writable_roots = if sandbox.writable_roots.is_empty() {
        vec![cwd.to_path_buf(), std::env::temp_dir()]
    } else {
        sandbox.writable_roots.clone()
    };
    Some(anie_sandbox::SandboxSpec {
        writable_roots,
        allow_network: sandbox.allow_network,
        require_kernel_support: sandbox.require_kernel_support,
    })
}

fn shell_command(command: &str) -> (&'static str, Vec<String>) {
    #[cfg(unix)]
    {
        ("/bin/sh", vec!["-c".into(), command.into()])
    }
    #[cfg(windows)]
    {
        ("cmd", vec!["/C".into(), command.into()])
    }
}

/// SIGKILL the child's whole process group so grandchildren (e.g.
/// `rustc` under `cargo check`) die with the shell. anie-cli carries
/// no `nix`/`libc` dependency, so this goes through the POSIX `kill`
/// utility instead of pulling one in for a single call (`bash.rs`
/// does the same kill via `nix` inside anie-tools). Best-effort: on
/// failure `kill_on_drop` still reaps the direct shell child.
async fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(unix)]
    {
        let _ = tokio::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .status()
            .await;
    }
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output()
            .await;
    }
}

/// Read the pipe to EOF (so the child never blocks on a full pipe),
/// retaining only the first `cap` bytes.
async fn drain_retaining_head<R>(mut reader: R, cap: usize) -> Vec<u8>
where
    R: AsyncReadExt + Unpin,
{
    let mut retained = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if retained.len() < cap {
                    let take = read.min(cap - retained.len());
                    retained.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
    retained
}

/// Hard byte ceiling on injected verify output. The line cap alone
/// is not enough: a low-newline tool (a minifier, a progress bar
/// writing \r) can pack hundreds of KB into 40 "lines" (2026-06-12
/// review). 8 KiB comfortably holds the first errors of any rustc /
/// tsc / pytest run.
const MAX_VERIFY_OUTPUT_BYTES: usize = 8 * 1024;

/// First `cap` lines of `text` (first-N — compile errors front-load),
/// additionally capped at [`MAX_VERIFY_OUTPUT_BYTES`], with a
/// one-line truncation note when anything was dropped.
fn first_lines(text: &str, cap: usize) -> String {
    let mut lines = text.lines();
    let head: Vec<&str> = lines.by_ref().take(cap).collect();
    let mut truncated = lines.next().is_some();
    let mut rendered = head.join("\n");
    if rendered.len() > MAX_VERIFY_OUTPUT_BYTES {
        let cut = (0..=MAX_VERIFY_OUTPUT_BYTES)
            .rev()
            .find(|i| rendered.is_char_boundary(*i))
            .unwrap_or(0);
        rendered.truncate(cut);
        truncated = true;
    }
    if truncated {
        rendered.push_str("\n[verify output truncated to first ");
        rendered.push_str(&cap.to_string());
        rendered.push_str(" lines / 8KiB]");
    }
    rendered
}

/// Render the ONE context-only reminder for an outcome. Success is a
/// single body line so the model can claim completion with evidence.
fn verify_reminder(command: &str, outcome: &VerifyOutcome, max_output_lines: usize) -> String {
    match outcome {
        VerifyOutcome::Passed => format!(
            "<system-reminder source=\"verify\">\nverify passed: {command}\n</system-reminder>"
        ),
        VerifyOutcome::Failed { cause, output } => format!(
            "<system-reminder source=\"verify\">\n`{command}` FAILED ({cause}) after your \
             edits.\nFirst errors ({max_output_lines}-line cap):\n{head}\nFix these before \
             proceeding; the harness re-runs this check after your next edit.\n</system-reminder>",
            head = first_lines(output, max_output_lines),
        ),
    }
}

/// Transcript/metrics event text for an outcome. Prefixes are stable:
/// [`VERIFY_EVENT_PREFIX`] / [`VERIFY_FAILURE_EVENT_PREFIX`].
fn verify_event_text(command: &str, outcome: &VerifyOutcome) -> String {
    match outcome {
        VerifyOutcome::Passed => format!("{VERIFY_EVENT_PREFIX} passed: {command}"),
        VerifyOutcome::Failed { cause, .. } => {
            format!("{VERIFY_FAILURE_EVENT_PREFIX} ({cause}): {command}")
        }
    }
}

/// Tracks which mutating successes have already been verified.
///
/// `floor` is the policy construction time: results older than the
/// run (a resumed session's history) never trigger a surprise
/// execution at step 0. After the first run, only results strictly
/// newer than the newest already-verified one re-trigger — at most
/// one run per `before_model` call, i.e. per model turn.
struct VerifyCursor {
    floor: u64,
    last_verified: Option<u64>,
}

impl VerifyCursor {
    /// If a run is due for `newest` (the newest mutating-success
    /// timestamp in the working context), claim it and return true.
    fn claim(&mut self, newest: u64) -> bool {
        let due = match self.last_verified {
            None => newest >= self.floor,
            Some(last) => newest > last,
        };
        if due {
            self.last_verified = Some(newest);
        }
        due
    }
}

/// The before-model policy: trigger + debounce + injection.
pub(crate) struct VerifyPolicy {
    runner: VerifyRunner,
    cursor: Mutex<VerifyCursor>,
    /// Optional transcript/metrics event channel. The reminder goes to
    /// the model; the event makes the run visible to the user and to
    /// `RunMetricsAccumulator` (same channel the loop-warning
    /// SystemMessages use).
    event_tx: Option<mpsc::Sender<AgentEvent>>,
}

impl VerifyPolicy {
    pub(crate) fn new(runner: VerifyRunner, event_tx: Option<mpsc::Sender<AgentEvent>>) -> Self {
        Self::with_floor(runner, event_tx, now_millis())
    }

    /// Test seam: an explicit baseline timestamp instead of "now".
    pub(crate) fn with_floor(
        runner: VerifyRunner,
        event_tx: Option<mpsc::Sender<AgentEvent>>,
        floor: u64,
    ) -> Self {
        Self {
            runner,
            cursor: Mutex::new(VerifyCursor {
                floor,
                last_verified: None,
            }),
            event_tx,
        }
    }
}

/// Newest timestamp among successful mutating-tool results in the
/// working context, if any.
fn newest_mutating_success(context: &[Message]) -> Option<u64> {
    context
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result)
                if !result.is_error && MUTATING_TOOLS.contains(&result.tool_name.as_str()) =>
            {
                Some(result.timestamp)
            }
            _ => None,
        })
        .max()
}

#[async_trait]
impl BeforeModelPolicy for VerifyPolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        let Some(newest) = newest_mutating_success(request.context) else {
            return BeforeModelResponse::Continue;
        };
        // Claim before running so the debounce holds even if the
        // command misbehaves.
        let claimed = self
            .cursor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .claim(newest);
        if !claimed {
            return BeforeModelResponse::Continue;
        }
        let outcome = self.runner.run().await;
        tracing::info!(
            target: "anie_cli::verify",
            command = self.runner.command(),
            passed = matches!(outcome, VerifyOutcome::Passed),
            "verify_command_ran"
        );
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx
                .send(AgentEvent::SystemMessage {
                    text: verify_event_text(self.runner.command(), &outcome),
                })
                .await;
        }
        let text = verify_reminder(
            self.runner.command(),
            &outcome,
            self.runner.max_output_lines,
        );
        BeforeModelResponse::AppendMessages(vec![Message::User(UserMessage {
            content: vec![ContentBlock::Text { text }],
            timestamp: now_millis(),
        })])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::ToolResultMessage;

    fn runner(command: &str, timeout_s: u64, max_output_lines: usize) -> VerifyRunner {
        VerifyRunner::new(
            command.to_string(),
            Duration::from_secs(timeout_s),
            max_output_lines,
            std::env::temp_dir(),
            None,
        )
    }

    fn tool_result(tool_name: &str, is_error: bool, timestamp: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: format!("c{timestamp}"),
            tool_name: tool_name.to_string(),
            content: vec![],
            details: serde_json::Value::Null,
            is_error,
            timestamp,
        })
    }

    fn model() -> &'static anie_provider::Model {
        use std::sync::OnceLock;
        static MODEL: OnceLock<anie_provider::Model> = OnceLock::new();
        MODEL.get_or_init(|| anie_provider::Model {
            id: "m".into(),
            name: "m".into(),
            provider: "p".into(),
            api: anie_provider::ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 1000,
            max_tokens: 100,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: anie_provider::ModelCompat::None,
        })
    }

    fn empty_usage() -> &'static anie_protocol::Usage {
        use std::sync::OnceLock;
        static USAGE: OnceLock<anie_protocol::Usage> = OnceLock::new();
        USAGE.get_or_init(anie_protocol::Usage::default)
    }

    fn request(context: &[Message]) -> BeforeModelRequest<'_> {
        static EMPTY: &[Message] = &[];
        BeforeModelRequest {
            context,
            generated_messages: EMPTY,
            model: model(),
            step_index: 0,
            run_usage: empty_usage(),
        }
    }

    fn appended_text(response: &BeforeModelResponse) -> String {
        match response {
            BeforeModelResponse::AppendMessages(messages) => {
                assert_eq!(messages.len(), 1, "exactly ONE injected message");
                match &messages[0] {
                    Message::User(user) => match &user.content[0] {
                        ContentBlock::Text { text } => text.clone(),
                        other => panic!("expected text content, got {other:?}"),
                    },
                    other => panic!("expected a user message, got {other:?}"),
                }
            }
            other => panic!("expected AppendMessages, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn verify_runs_once_after_a_turn_containing_successful_edits() {
        let policy = VerifyPolicy::with_floor(runner("true", 5, 40), None, 100);
        let context = vec![tool_result("edit", false, 100)];
        let text = appended_text(&policy.before_model(request(&context)).await);
        assert!(text.contains("verify passed: true"), "{text}");
        // Same context again: debounced — at most once per the edits
        // already verified.
        assert_eq!(
            policy.before_model(request(&context)).await,
            BeforeModelResponse::Continue
        );
        // A NEWER successful write re-arms the trigger.
        let context = vec![tool_result("edit", false, 100), tool_result("write", false, 200)];
        assert!(matches!(
            policy.before_model(request(&context)).await,
            BeforeModelResponse::AppendMessages(_)
        ));
    }

    #[tokio::test]
    async fn verify_does_not_run_when_no_mutating_tool_succeeded() {
        let policy = VerifyPolicy::with_floor(runner("true", 5, 40), None, 0);
        // A FAILED edit and a successful non-mutating tool: no trigger.
        let context = vec![
            tool_result("edit", true, 100),
            tool_result("grep", false, 200),
        ];
        assert_eq!(
            policy.before_model(request(&context)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn stale_edits_from_before_the_run_do_not_trigger_at_step_zero() {
        // Resumed session: history holds old successful edits. The
        // floor (policy construction time) must keep them from firing
        // a surprise verify at step 0.
        let policy = VerifyPolicy::with_floor(runner("true", 5, 40), None, 1_000);
        let context = vec![tool_result("edit", false, 999)];
        assert_eq!(
            policy.before_model(request(&context)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn verify_failure_injects_capped_first_n_lines_of_output() {
        // 6 numbered lines on stderr, cap at 3: the head survives, the
        // tail is dropped with a truncation note.
        let command = "for i in 1 2 3 4 5 6; do echo \"line $i\" 1>&2; done; exit 7";
        let policy = VerifyPolicy::with_floor(runner(command, 5, 3), None, 100);
        let context = vec![tool_result("apply_patch", false, 100)];
        let text = appended_text(&policy.before_model(request(&context)).await);
        assert!(text.contains("FAILED (exit 7)"), "{text}");
        assert!(text.contains("line 1") && text.contains("line 3"), "{text}");
        assert!(!text.contains("line 4"), "cap is first-N: {text}");
        assert!(text.contains("truncated to first 3 lines"), "{text}");
    }

    #[tokio::test]
    async fn verify_timeout_kills_process_group_and_reports_timeout() {
        let start = std::time::Instant::now();
        let policy = VerifyPolicy::with_floor(runner("sleep 30", 1, 40), None, 100);
        let context = vec![tool_result("edit", false, 100)];
        let text = appended_text(&policy.before_model(request(&context)).await);
        assert!(text.contains("timeout after 1s"), "{text}");
        assert!(text.contains("process group killed"), "{text}");
        // The group kill returns promptly — nowhere near the 30s sleep.
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn verify_emits_run_and_failure_events_with_stable_prefixes() {
        let (tx, mut rx) = mpsc::channel(8);
        let policy = VerifyPolicy::with_floor(runner("exit 2", 5, 40), Some(tx), 100);
        let context = vec![tool_result("write", false, 100)];
        let _ = policy.before_model(request(&context)).await;
        let Some(AgentEvent::SystemMessage { text }) = rx.recv().await else {
            panic!("expected a SystemMessage event");
        };
        assert!(text.starts_with(VERIFY_FAILURE_EVENT_PREFIX), "{text}");
        assert!(text.starts_with(VERIFY_EVENT_PREFIX), "{text}");
    }

    #[test]
    fn first_lines_passes_short_output_through_unmarked() {
        assert_eq!(first_lines("a\nb", 3), "a\nb");
    }

    #[test]
    fn sandbox_spec_disabled_config_yields_none() {
        let config = anie_config::SandboxToolConfig::default();
        assert!(sandbox_spec(&config, Path::new("/w")).is_none());
    }

    // ---- end-to-end: edit -> harness verify, context-only ----

    use std::sync::Arc;

    use anie_agent::{AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry};
    use anie_protocol::{AssistantMessage, StopReason, ToolCall, Usage};
    use anie_provider::{
        ApiKind, ProviderError, ProviderRegistry, RequestOptionsResolver, ResolvedRequestOptions,
        mock::{MockProvider, MockStreamScript},
    };

    struct StaticResolver;

    #[async_trait]
    impl RequestOptionsResolver for StaticResolver {
        async fn resolve(
            &self,
            _model: &anie_provider::Model,
            _context: &[Message],
        ) -> Result<ResolvedRequestOptions, ProviderError> {
            Ok(ResolvedRequestOptions::default())
        }
    }

    fn assistant_write_call(path: &str) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".into(),
                name: "write".into(),
                arguments: serde_json::json!({ "path": path, "content": "hello" }),
            })],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "mock".into(),
            model: "m".into(),
            timestamp: 1,
            reasoning_details: None,
        }
    }

    fn assistant_done() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text { text: "done".into() }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "m".into(),
            timestamp: 1,
            reasoning_details: None,
        }
    }

    fn contains_verify_reminder(messages: &[Message]) -> bool {
        messages.iter().any(|message| match message {
            Message::User(user) => user.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text }
                    if text.contains("<system-reminder source=\"verify\">"))
            }),
            _ => false,
        })
    }

    #[tokio::test]
    async fn verify_result_message_is_context_only_not_persisted() {
        // The model writes a file on turn 1; the policy runs the verify
        // command before turn 2 and injects the reminder into the run
        // context — but it must NOT appear in the controller-persisted
        // generated_messages.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(anie_tools::WriteTool::new(dir.path())));

        let mut providers = ProviderRegistry::new();
        providers.register(
            ApiKind::OpenAICompletions,
            Box::new(MockProvider::new(vec![
                MockStreamScript::from_message(assistant_write_call("a.txt")),
                MockStreamScript::from_message(assistant_done()),
            ])),
        );

        let policy = VerifyPolicy::with_floor(runner("true", 5, 40), None, 0);
        let config = AgentLoopConfig::new(
            model().clone(),
            "system".into(),
            anie_provider::ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver),
        )
        .with_before_model_policy(Arc::new(policy));

        let agent = AgentLoop::new(Arc::new(providers), Arc::new(tools), config);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let result = agent
            .run(
                vec![Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "write a.txt".into(),
                    }],
                    timestamp: 1,
                })],
                Vec::new(),
                tx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;

        assert!(
            result.terminal_error.is_none(),
            "run should complete: {:?}",
            result.terminal_error
        );
        assert!(
            contains_verify_reminder(&result.final_context),
            "verify reminder must be injected into the run context"
        );
        assert!(
            !contains_verify_reminder(&result.generated_messages),
            "verify reminder is context-only and must not be persisted"
        );
    }
}
