# Phase 6: Hardening (Weeks 11–12)

**Goal:** Make anie-rs production-ready. Error recovery, retry logic, graceful shutdown, cross-platform support, and release optimization. By the end of Phase 6, you should be confident giving anie-rs to other people.

---

## Sub-phase 6.1: Error Recovery and Retry Logic

**Duration:** Days 1–3

### Retry Strategy

Mirror pi's auto-retry behavior: retry on transient errors with exponential backoff.

```rust
pub struct RetryConfig {
    pub max_retries: u32,           // default: 3
    pub initial_delay_ms: u64,      // default: 1000
    pub max_delay_ms: u64,          // default: 30000
    pub backoff_multiplier: f64,    // default: 2.0
    pub jitter: bool,               // default: true
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay_ms: 1000,
            max_delay_ms: 30000,
            backoff_multiplier: 2.0,
            jitter: true,
        }
    }
}
```

### Classifying Retryable Errors

```rust
impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } => true,
            ProviderError::Http { status, .. } => {
                matches!(status, 429 | 500 | 502 | 503 | 529)
            }
            ProviderError::Stream(_) => true,  // Network issues
            ProviderError::ContextOverflow(_) => false,  // Not retryable directly
            ProviderError::Auth(_) => false,
            ProviderError::Request(_) => false,
            ProviderError::Other(_) => false,
        }
    }

    pub fn retry_after_ms(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimited { retry_after_ms } => *retry_after_ms,
            _ => None,
        }
    }
}
```

### Retry Wrapper

```rust
pub async fn with_retry<F, Fut, T>(
    config: &RetryConfig,
    event_tx: &mpsc::Sender<AgentEvent>,
    mut operation: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut attempt = 0;
    let mut delay = config.initial_delay_ms;

    loop {
        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) if e.is_retryable() && attempt < config.max_retries => {
                attempt += 1;

                // Use retry-after if provided, otherwise exponential backoff
                let wait_ms = e.retry_after_ms().unwrap_or(delay);

                tracing::warn!(
                    "Provider error (attempt {}/{}): {}. Retrying in {}ms...",
                    attempt,
                    config.max_retries,
                    e,
                    wait_ms,
                );

                // Notify the TUI
                let _ = event_tx.send(AgentEvent::RetryScheduled {
                    attempt,
                    max_retries: config.max_retries,
                    delay_ms: wait_ms,
                    error: e.to_string(),
                }).await;

                // Apply jitter: ±25%
                let actual_delay = if config.jitter {
                    let jitter = (wait_ms as f64 * 0.25) as u64;
                    let offset = rand::random::<u64>() % (jitter * 2 + 1);
                    wait_ms.saturating_sub(jitter) + offset
                } else {
                    wait_ms
                };

                tokio::time::sleep(Duration::from_millis(actual_delay)).await;

                // Exponential backoff for next attempt
                delay = (delay as f64 * config.backoff_multiplier) as u64;
                delay = delay.min(config.max_delay_ms);
            }
            Err(e) => return Err(e),
        }
    }
}
```

### Add RetryScheduled Event

```rust
pub enum AgentEvent {
    // ... existing variants ...
    RetryScheduled {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
}
```

### TUI Handling

```rust
AgentEvent::RetryScheduled { attempt, max_retries, delay_ms, error } => {
    self.output_pane.add_system_message(&format!(
        "⟳ Retrying ({}/{}) in {:.1}s: {}",
        attempt, max_retries, delay_ms as f64 / 1000.0, error,
    ));
}
```

### Integrate Retry into Agent Loop

Wrap the full request-construction step in retry so every attempt gets fresh auth/options and a fresh stream:

```rust
// In the agent loop's streaming section
let stream = with_retry(&self.retry_config, &event_tx, || async {
    let request = self.request_options_resolver
        .resolve(&self.config.model, &context)
        .await?;

    let mut model = self.config.model.clone();
    if let Some(base_url) = request.base_url_override {
        model.base_url = base_url;
    }

    let options = StreamOptions {
        api_key: request.api_key,
        temperature: None,
        max_tokens: Some(model.max_tokens),
        thinking: self.config.thinking,
        headers: request.headers,
    };

    let provider = self.provider_registry.get(&model.api).unwrap();
    provider.stream(&model, llm_context.clone(), options)
}).await?;
```

Because `ProviderStream` now yields `Result<ProviderEvent, ProviderError>`, mid-stream failures stay structured and can participate in the same retry/error-reporting path.

### Context Overflow Recovery

Special handling for context overflow errors (not retryable, but recoverable via compaction):

```rust
match &error {
    ProviderError::ContextOverflow(msg) => {
        tracing::warn!("Context overflow: {}. Attempting compaction...", msg);

        let _ = event_tx.send(AgentEvent::CompactionStart).await;

        match auto_compact_aggressive(
            session,
            config,
            self.request_options_resolver.as_ref(),
            provider_registry,
        ).await {
            Ok(Some(result)) => {
                let _ = event_tx.send(AgentEvent::CompactionEnd { ... }).await;

                let session_ctx = session.build_context();
                *context = session_ctx.messages
                    .into_iter()
                    .map(|m| m.message)
                    .collect();

                // Retry the LLM call (not the entire outer workflow)
                continue;
            }
            Ok(None) => {
                let _ = event_tx.send(AgentEvent::AgentEnd { ... }).await;
                return new_messages;
            }
            Err(e) => {
                tracing::error!("Compaction failed during overflow recovery: {}", e);
                let _ = event_tx.send(AgentEvent::AgentEnd { ... }).await;
                return new_messages;
            }
        }
    }
    _ => { /* non-recoverable error */ }
}
```

### Parse Retry-After Headers

Extract retry-after from provider response headers:

```rust
fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
    response.headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            // Try as seconds (integer)
            if let Ok(secs) = s.parse::<u64>() {
                return Some(secs * 1000);
            }
            // Try as HTTP date (rare)
            None
        })
}
```

Update the provider implementations to extract `retry-after` from 429 responses and include it in `ProviderError::RateLimited`.

### Tests

1. **Retry on 429:** Mock provider returns 429 twice, then succeeds. Verify 3 attempts.
2. **Retry on 529:** Anthropic overloaded error, verify retry.
3. **No retry on 401:** Auth error, verify immediate failure.
4. **Max retries exceeded:** Verify error after `max_retries` attempts.
5. **Retry-after header:** Verify delay uses header value.
6. **Context overflow recovery:** Simulate overflow, verify compaction + retry.

### Acceptance Criteria

- Transient errors (429, 529, 5xx) are retried automatically.
- Retry delay uses exponential backoff with jitter.
- Context overflow triggers compaction and retry.
- TUI shows retry status to the user.

---

## Sub-phase 6.2: Graceful Shutdown

**Duration:** Days 3–5

### Shutdown Triggers

1. **User quits:** Ctrl+D, `/quit`, or closing the terminal.
2. **Ctrl+C:** First press cancels agent, second press while idle quits.
3. **Panic:** Panic hook restores terminal.
4. **Signal:** SIGTERM, SIGHUP (Unix).

### Shutdown Sequence

```rust
async fn shutdown(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    session: &mut SessionManager,
    cancel: &CancellationToken,
) -> Result<()> {
    tracing::info!("Shutting down...");

    // 1. Cancel any in-flight agent runs
    cancel.cancel();

    // 2. Wait briefly for tool executions to drain
    tokio::time::timeout(Duration::from_secs(3), async {
        // Wait for the agent task to complete
        // The agent loop checks CancellationToken and should exit promptly
    }).await.ok();

    // 3. Flush session file
    session.flush()?;

    // 4. Restore terminal
    restore_terminal(terminal)?;

    tracing::info!("Shutdown complete.");
    Ok(())
}
```

### Signal Handling (Unix)

```rust
#[cfg(unix)]
async fn setup_signal_handlers(cancel: CancellationToken, quit_tx: mpsc::Sender<()>) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sighup = signal(SignalKind::hangup()).unwrap();

    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM");
            cancel.cancel();
            let _ = quit_tx.send(()).await;
        }
        _ = sighup.recv() => {
            tracing::info!("Received SIGHUP");
            cancel.cancel();
            let _ = quit_tx.send(()).await;
        }
    }
}
```

### Ctrl+C Double-Press Logic

```rust
fn handle_ctrl_c(&mut self) {
    match self.agent_state {
        AgentUiState::Idle => {
            // First Ctrl+C while idle → quit
            self.should_quit = true;
        }
        AgentUiState::Streaming | AgentUiState::ToolExecuting { .. } => {
            if self.last_ctrl_c.map_or(true, |t| t.elapsed() > Duration::from_secs(2)) {
                // First Ctrl+C during execution → abort agent
                self.cancel.cancel();
                self.last_ctrl_c = Some(Instant::now());
                self.output_pane.add_system_message("Aborting... (press Ctrl+C again to quit)");
            } else {
                // Second Ctrl+C within 2 seconds → force quit
                self.should_quit = true;
            }
        }
    }
}
```

### Process Tree Cleanup

When the agent is cancelled, ensure all spawned child processes are killed:

```rust
// In BashTool, register a cleanup handler
impl Drop for BashToolChild {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_process_tree(pid);
        }
    }
}
```

### Terminal Restoration on Panic (Enhanced)

Improve the Phase 3 panic hook with more robust restoration:

```rust
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // Restore terminal state
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
        );

        // Print a user-friendly message
        eprintln!("\n\x1b[31manie panicked!\x1b[0m This is a bug.");
        eprintln!("Please report it at: https://github.com/example/anie-rs/issues\n");

        original_hook(panic_info);
    }));
}
```

### Acceptance Criteria

- Ctrl+D exits cleanly with terminal restoration.
- Ctrl+C during agent run aborts, second Ctrl+C quits.
- SIGTERM triggers clean shutdown.
- Panic restores terminal and shows a helpful message.
- All child processes are cleaned up on exit.
- Session file is flushed before exit.

---

## Sub-phase 6.3: Error Handling Audit

**Duration:** Days 5–6

Systematically review every error path in the codebase.

### Audit Checklist

| Component | Error Scenario | Expected Behavior |
|---|---|---|
| Provider HTTP | Connection refused | Retry with backoff |
| Provider HTTP | DNS resolution failure | Error message, no retry |
| Provider HTTP | TLS handshake failure | Error with cert info |
| Provider SSE | Malformed SSE event | Skip event, log warning |
| Provider SSE | Stream drops mid-response | Retry from scratch |
| Provider SSE | Invalid JSON in event data | Skip event, log warning |
| Auth | auth.json missing | Fall through to env var |
| Auth | auth.json malformed | Warn, fall through |
| Auth | auth.json permission error | Warn, fall through |
| Config | config.toml missing | Use defaults |
| Config | config.toml malformed | Error with line number |
| Config | Unknown config keys | Warn, ignore |
| Session | Session file missing | Error for --resume |
| Session | Session file malformed | Skip bad lines, continue |
| Session | Disk full during write | Error, but don't lose in-memory state |
| Session | Permission denied | Error with path |
| Tool (read) | File not found | Error result to LLM |
| Tool (read) | Permission denied | Error result to LLM |
| Tool (read) | Binary file | Message to LLM |
| Tool (write) | Permission denied | Error result to LLM |
| Tool (write) | Disk full | Error result to LLM |
| Tool (edit) | Match not found | Error result to LLM |
| Tool (edit) | Duplicate match | Error result to LLM |
| Tool (bash) | Command not found | Error result to LLM |
| Tool (bash) | Timeout | Error result with output |
| Tool (bash) | OOM kill | Error result |
| TUI | Terminal resize | Re-render at new size |
| TUI | Terminal detach (SSH) | Clean shutdown |
| Agent | All tools fail | Report to user, don't crash |
| Compaction | LLM summarization fails | Skip compaction, continue |

### Error Message Quality

Audit all error messages for:
- **Actionability:** Does the message tell the user what to do? Bad: `"Error: 401"`. Good: `"Authentication failed for Anthropic. Check your API key with: anie --provider anthropic --api-key <key>"`.
- **Context:** Does the message include relevant context? Include the provider name, model ID, file path, etc.
- **No panics in production paths:** Replace any remaining `unwrap()` or `expect()` in non-test code with proper error handling.

### Implementation

```rust
// Replace generic error messages with specific ones
// Before:
Err(anyhow!("HTTP error"))

// After:
Err(ProviderError::Http {
    status: response.status().as_u16(),
    body: response.text().await.unwrap_or_default(),
}.into())
```

### Acceptance Criteria

- Every error scenario in the audit table is handled.
- No `unwrap()` or `expect()` in non-test code (or each is justified with a comment).
- Error messages are actionable.
- The agent never crashes on provider errors, only reports them.

---

## Sub-phase 6.4: Cross-Platform Testing

**Duration:** Days 6–8

### Platform Matrix

| Platform | Priority | Notes |
|---|---|---|
| Linux x86_64 | P0 | Primary development platform |
| macOS aarch64 | P0 | Most common Mac |
| macOS x86_64 | P1 | Older Macs |
| Windows x86_64 | P2 | Basic functionality |

### Linux-Specific Issues

- **Shell detection:** `$SHELL` env var. Fallback to `/bin/bash`, then `/bin/sh`.
- **Process groups:** `setsid` for process group creation, `kill(-pgid, SIGKILL)` for cleanup. This is already handled in Phase 1 BashTool.
- **File permissions:** `auth.json` mode 0600. Session dir mode 0700.
- **Terminal:** Most terminals support full crossterm features.

### macOS-Specific Issues

- **Shell detection:** Default shell is `zsh` since Catalina. `$SHELL` should return `/bin/zsh`.
- **Process groups:** Same as Linux (POSIX).
- **Keychain:** Not used in v1 (API keys in auth.json). Note for v2.
- **Terminal:** macOS Terminal.app has limited ANSI support. Test with iTerm2 and Terminal.app.

### Windows-Specific Issues

- **Shell detection:** Use `cmd.exe` or PowerShell. Check `$env:COMSPEC` for cmd, detect PowerShell availability.
  ```rust
  #[cfg(windows)]
  fn get_shell() -> (String, Vec<String>) {
      if let Ok(ps) = which::which("pwsh") {
          (ps.to_string_lossy().into(), vec!["-NoProfile".into(), "-Command".into()])
      } else if let Ok(ps) = which::which("powershell") {
          (ps.to_string_lossy().into(), vec!["-NoProfile".into(), "-Command".into()])
      } else {
          (std::env::var("COMSPEC").unwrap_or("cmd.exe".into()), vec!["/C".into()])
      }
  }
  ```
- **Process groups:** Windows doesn't have Unix process groups. Use `CREATE_NEW_PROCESS_GROUP` flag and `GenerateConsoleCtrlEvent` or `TerminateProcess` for cleanup.
  ```rust
  #[cfg(windows)]
  fn kill_process_tree(pid: u32) {
      use std::process::Command;
      let _ = Command::new("taskkill")
          .args(&["/F", "/T", "/PID", &pid.to_string()])
          .output();
  }
  ```
- **File permissions:** Windows doesn't have Unix permissions. The `0600` mode is a no-op. Consider using ACLs for auth.json, but defer to v2.
- **Paths:** Use `PathBuf` everywhere. Never hardcode `/`. Use `dirs::home_dir()` for home directory.
- **Line endings:** The edit tool must handle CRLF correctly (already done in Phase 5).
- **Terminal:** Windows Terminal supports full ANSI. Legacy cmd.exe needs `crossterm::terminal::enable_virtual_terminal_processing()`.

### Test Plan

For each platform:
1. `cargo build --release` succeeds.
2. `cargo test --workspace` passes.
3. `anie "say hello"` (print mode) works.
4. Interactive TUI launches and renders correctly.
5. Read, write, edit, bash tools work.
6. Session persistence works.
7. Ctrl+C cancels, Ctrl+D quits.

### CI Setup

```yaml
# GitHub Actions (example)
jobs:
  build:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --release
      - run: cargo test --workspace
```

### Acceptance Criteria

- Builds and tests pass on Linux, macOS, and Windows.
- Shell detection works on all platforms.
- Process cleanup works on all platforms.
- File paths work on all platforms.

---

## Sub-phase 6.5: Performance Optimization

**Duration:** Days 8–9

### Profiling

Use `cargo flamegraph` or `perf` to identify bottlenecks. Focus areas:

1. **TUI rendering:** Each frame should take < 5ms. If longer, check line wrapping and scroll offset computation.
2. **SSE parsing:** Should be minimal — just JSON deserialization.
3. **Session file I/O:** Append-only writes are fast. Reads at startup may be slow for very large sessions.

### TUI Optimizations

```rust
// Cache line wrapping for non-streaming blocks
struct CachedBlock {
    block: RenderedBlock,
    cached_lines: Option<(u16, Vec<Line<'static>>)>, // (width, lines)
}

impl CachedBlock {
    fn get_lines(&mut self, width: u16) -> &[Line] {
        if self.cached_lines.as_ref().map_or(true, |(w, _)| *w != width) {
            let lines = self.block.wrap_to_width(width);
            self.cached_lines = Some((width, lines));
        }
        &self.cached_lines.as_ref().unwrap().1
    }
}
```

### Session Loading Optimization

For large sessions (10,000+ entries), loading at startup can be slow. Optimize:

```rust
impl SessionManager {
    pub fn open_session_lazy(path: &Path) -> Result<Self> {
        // Read only the header and last N entries initially
        // Load the full file on demand (when navigating history)
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(&file);

        // Read header (first line)
        let mut first_line = String::new();
        reader.read_line(&mut first_line)?;
        let header: SessionHeader = serde_json::from_str(&first_line)?;

        // Seek to end, read backwards to find recent entries
        // (Or just read the whole thing — JSONL files are typically small)
        // For v1, just read everything. Optimize in v2 if needed.
        // ...
    }
}
```

### Binary Size

```toml
# Cargo.toml [profile.release]
[profile.release]
lto = "fat"
codegen-units = 1
strip = "symbols"
opt-level = "z"  # Optimize for size. Use "3" for speed.
panic = "abort"   # Smaller binary, no unwinding
```

Measure binary size with `cargo bloat` to identify large dependencies. Typical targets:
- Linux (musl static): < 20 MB
- macOS: < 15 MB
- Windows: < 20 MB

### Acceptance Criteria

- TUI renders at 60 fps with no noticeable lag.
- Session loading for 1000-entry files takes < 500ms.
- Release binary size is reasonable (< 20 MB).

---

## Sub-phase 6.6: Logging and Diagnostics

**Duration:** Day 9

### Structured Logging

Configure `tracing` with useful defaults:

```rust
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("anie=info,warn"));

    // In TUI mode, log to a file (stdout is the terminal)
    let log_dir = dirs::home_dir().unwrap().join(".anie/logs");
    std::fs::create_dir_all(&log_dir).ok();

    let file_appender = tracing_appender::rolling::daily(&log_dir, "anie.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt::layer().with_writer(non_blocking).with_ansi(false))
        .init();
}
```

**Key log points:**
- Provider requests: model, token count, headers (sanitized).
- Provider responses: status code, usage, stop reason.
- Tool executions: tool name, args (truncated), duration, result status.
- Errors: full error chain.
- Compaction: tokens before/after, messages discarded.

**Sanitization:** Never log API keys. Log only the last 4 characters for identification:

```rust
fn sanitize_key(key: &str) -> String {
    if key.len() > 8 {
        format!("...{}", &key[key.len()-4..])
    } else {
        "***".to_string()
    }
}
```

### Acceptance Criteria

- Logs are written to `~/.anie/logs/anie.log`.
- API keys are never logged.
- `RUST_LOG=anie=debug` shows detailed provider interaction.

---

## Sub-phase 6.7: Release Preparation

**Duration:** Days 10–12

### Documentation

1. **README.md** — Installation, quick start, configuration reference.
2. **CHANGELOG.md** — Version history.
3. **LICENSE** — Choose and include.

### Installation Methods

1. **Cargo install:**
   ```bash
   cargo install --git https://github.com/example/anie-rs
   ```

2. **Pre-built binaries:** Build with CI for Linux (musl), macOS (aarch64, x86_64), Windows.

3. **Homebrew (optional):**
   ```ruby
   brew install anie
   ```

### Release Build Script

```bash
#!/bin/bash
# build-release.sh

set -euo pipefail

VERSION=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
echo "Building anie v${VERSION}"

# Linux (static musl)
cross build --release --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/anie "dist/anie-${VERSION}-linux-x86_64"

# macOS ARM
cargo build --release --target aarch64-apple-darwin
cp target/aarch64-apple-darwin/release/anie "dist/anie-${VERSION}-macos-aarch64"

# macOS Intel
cargo build --release --target x86_64-apple-darwin
cp target/x86_64-apple-darwin/release/anie "dist/anie-${VERSION}-macos-x86_64"

# Windows
cross build --release --target x86_64-pc-windows-gnu
cp target/x86_64-pc-windows-gnu/release/anie.exe "dist/anie-${VERSION}-windows-x86_64.exe"

echo "Built all targets in dist/"
ls -la dist/
```

### Final Verification Checklist

| # | Check | Status |
|---|---|---|
| 1 | `cargo test --workspace` passes | ☐ |
| 2 | `cargo clippy --workspace -- -D warnings` passes | ☐ |
| 3 | `cargo fmt --all -- --check` passes | ☐ |
| 4 | Release binary runs on Linux | ☐ |
| 5 | Release binary runs on macOS | ☐ |
| 6 | Release binary runs on Windows | ☐ |
| 7 | First-run onboarding works | ☐ |
| 8 | Interactive mode works end-to-end | ☐ |
| 9 | Print mode works | ☐ |
| 10 | Session resume works | ☐ |
| 11 | Compaction works | ☐ |
| 12 | All three providers work (Anthropic, OpenAI, Google) | ☐ |
| 13 | Ctrl+C / Ctrl+D work correctly | ☐ |
| 14 | Panic hook restores terminal | ☐ |
| 15 | Logs written to ~/.anie/logs/ | ☐ |
| 16 | Binary size < 20 MB | ☐ |
| 17 | No `unwrap()` in non-test code | ☐ |
| 18 | README documentation complete | ☐ |

### Acceptance Criteria

- All 18 checks pass.
- Release binaries built for all 4 targets.
- README provides clear installation and usage instructions.

---

## Phase 6 Milestones Checklist

| # | Milestone | Verified By |
|---|---|---|
| 1 | Retry logic handles 429/529/5xx with backoff | Unit tests + manual |
| 2 | Context overflow triggers compaction + retry | Integration test |
| 3 | Graceful shutdown on all signals | Manual test |
| 4 | Terminal restored on panic | Manual test |
| 5 | All child processes cleaned up on exit | Manual test |
| 6 | Error messages are actionable | Code review |
| 7 | Builds on Linux, macOS, Windows | CI |
| 8 | Tests pass on all platforms | CI |
| 9 | TUI renders smoothly (60 fps target) | Manual test |
| 10 | Release binary < 20 MB | CI measurement |
| 11 | Logs written without API key exposure | Code review |
| 12 | Final 18-point verification passes | Manual checklist |
