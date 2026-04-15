# Shell Escape (`!`) in the TUI

**Date:** 2026-04-15
**Status:** First draft / proposal

---

## Overview

This document proposes adding a `!` prefix in the TUI input pane that lets the user run shell commands directly, without routing them through the LLM. This is a common pattern in tools like vim (`:!cmd`), gdb (`!cmd`), and many REPLs.

**Example usage:**
```
> !ls -la
> !git status
> !cargo test -p anie-tui
> !cat src/main.rs | head -20
```

---

## Can this be done in a ratatui-based TUI?

**Yes, absolutely.** There are two viable approaches, and both work well with ratatui + crossterm.

### Approach A: Run the command and capture output into the transcript

The TUI stays in alternate-screen mode. The command runs in the background. Its stdout/stderr is captured and displayed as a system message block in the output pane, just like tool results are displayed today.

**Advantages:**
- the user never leaves the TUI
- output is scrollable in the transcript
- output is visually consistent with tool-result blocks
- simplest to implement

**Disadvantages:**
- interactive commands (e.g., `vim`, `htop`, `less`) won't work because they need a real terminal
- long-running commands need cancellation support

### Approach B: Suspend the TUI, run the command in the real terminal, then resume

The TUI temporarily leaves alternate-screen mode and restores the normal terminal. The command runs with full terminal access. When it exits, the TUI re-enters alternate-screen mode and resumes.

**Advantages:**
- interactive commands work naturally
- the user sees real terminal output as they would in a normal shell
- familiar to vim/less users

**Disadvantages:**
- the output is not captured into the transcript (unless we also tee it)
- the TUI flickers during the suspend/resume transition
- more complex terminal state management

### Recommendation

**Start with Approach A** (capture output into the transcript). It covers the primary use case — quick shell commands for inspecting state — and fits naturally into the existing rendering infrastructure. Approach B can be added later as `!!` or a `/shell` command if interactive shell access is needed.

---

## How it would work

### User-facing behavior

1. The user types `!git status` in the input pane and presses Enter.
2. The TUI immediately displays a system message or tool-like block showing the command.
3. The command runs asynchronously. Output streams into the block as it arrives.
4. When the command completes, the exit code and elapsed time are shown.
5. The command output is **not** sent to the LLM as context — it is a local-only operation.
6. If the user presses Ctrl+C while a shell escape is running, the command is killed.

### What it should NOT do

- It should not add the command or its output to the agent conversation context.
- It should not persist the output in the session JSONL file.
- It should not count toward token usage or context window estimates.
- It should not block the user from scrolling the transcript while the command runs.

---

## Implementation plan

### Layer 1: TUI detection (`anie-tui`)

In `App::handle_submit()`, add a `!` prefix check alongside the existing `/` slash-command check:

```rust
fn handle_submit(&mut self, text: String) {
    let trimmed = text.trim();
    if trimmed.starts_with('/') {
        self.handle_slash_command(trimmed);
    } else if trimmed.starts_with('!') {
        let command = trimmed.strip_prefix('!').unwrap_or("").trim();
        if !command.is_empty() {
            let _ = self.action_tx.try_send(UiAction::ShellEscape(command.to_string()));
        }
    } else {
        let _ = self.action_tx.try_send(UiAction::SubmitPrompt(text));
    }
}
```

Add a new variant to `UiAction`:

```rust
pub enum UiAction {
    // ... existing variants ...
    /// Run a user shell command (not sent to LLM).
    ShellEscape(String),
}
```

### Layer 2: Controller execution (`anie-cli`)

The controller receives `UiAction::ShellEscape(command)` and:

1. Displays a system message showing the command being run.
2. Spawns the command using `tokio::process::Command` (similar to `BashTool`).
3. Captures stdout/stderr.
4. Sends the output back to the TUI as a system message or a dedicated shell-result block.
5. Does **not** persist the command or output to the session.
6. Does **not** add it to the agent context.

```rust
UiAction::ShellEscape(command) => {
    let _ = event_tx.send(AgentEvent::SystemMessage {
        text: format!("$ {command}"),
    }).await;

    match run_shell_command(&command, &self.current_cwd).await {
        Ok(output) => {
            let _ = event_tx.send(AgentEvent::SystemMessage {
                text: output,
            }).await;
        }
        Err(error) => {
            let _ = event_tx.send(AgentEvent::SystemMessage {
                text: format!("Command failed: {error}"),
            }).await;
        }
    }
}
```

The `run_shell_command` helper would be a simplified version of `BashTool::execute` — spawn a process, collect output, apply truncation limits, return the result. It does not need tool-call IDs, cancellation tokens, or update channels.

### Layer 3: Optional — dedicated rendering block

For the first version, using `SystemMessage` blocks is sufficient. The command and output will render in the same muted gray as other system messages.

For a nicer version later, a dedicated `ShellEscape` block type could be added to `RenderedBlock`:

```rust
RenderedBlock::ShellEscape {
    command: String,
    output: String,
    exit_code: Option<i32>,
    elapsed: Option<Duration>,
}
```

This would render with the same boxed format used for tool calls:

```
┌─ $ git status ──────────────────────────────┐
│ On branch main                               │
│ nothing to commit, working tree clean        │
│                                              │
│ Took 0.1s                                    │
└──────────────────────────────────────────────┘
```

This is optional for the first implementation.

---

## Cancellation

While a shell escape is running, the user should be able to cancel it. Two options:

### Option A: Ctrl+C cancels the shell command (not the agent)

If a shell escape is active, Ctrl+C kills the running shell process instead of aborting an agent run. This requires the controller to track whether a shell escape is active.

### Option B: Shell escapes are non-cancellable but time-limited

Apply a default timeout (e.g., 30 seconds). If the command doesn't finish in time, it is killed automatically. Simpler to implement.

**Recommendation:** Start with Option B (timeout). Add Option A later if needed.

---

## State considerations

### Agent state during shell escape

The shell escape should be allowed both while idle and while streaming. If the agent is streaming, the shell command runs independently — it is not part of the agent conversation.

This means the controller needs to handle `ShellEscape` in both idle and active states. The simplest approach: handle it outside the main agent-run loop, as a fire-and-forget task.

### Concurrent shell escapes

For simplicity, only allow one shell escape at a time. If the user submits `!cmd` while another is running, queue it or reject it with a message.

---

## Files that would change

| File | Change |
|---|---|
| `crates/anie-tui/src/app.rs` | `!` prefix detection in `handle_submit`, new `UiAction::ShellEscape` variant |
| `crates/anie-cli/src/controller.rs` | Handle `UiAction::ShellEscape`, spawn and capture shell command |
| `crates/anie-tui/src/output.rs` | Optional: dedicated `ShellEscape` rendered block type |
| `crates/anie-tui/src/tests.rs` | Test that `!` input emits `ShellEscape` action |
| `crates/anie-cli/src/controller.rs` | Test that `ShellEscape` action produces system messages |

No changes needed to:
- `anie-agent` (not part of the agent loop)
- `anie-session` (not persisted)
- `anie-protocol` (no new event types needed for v1)
- `anie-provider` (no provider interaction)
- `anie-tools` (this is a controller-level feature, not a tool)

---

## Separation of concerns

This feature fits cleanly into the existing architecture:

- **TUI** detects the `!` prefix and emits a `UiAction` — it does not execute anything.
- **Controller** receives the action, runs the command, and sends results back as events — it owns execution.
- **Agent loop** is not involved — this is not an LLM interaction.
- **Session** is not involved — nothing is persisted.

The same UI/orchestration split that governs slash commands governs shell escapes.

---

## Relationship to `BashTool`

The shell escape is intentionally **not** implemented as a `BashTool` invocation. Key differences:

| Aspect | `BashTool` (agent) | `!` shell escape (user) |
|---|---|---|
| Initiated by | LLM | User |
| Added to context | Yes | No |
| Persisted to session | Yes | No |
| Rendered as | Tool call block | System message (or shell block) |
| Cancellation | Via agent cancellation token | Via timeout or dedicated cancel |
| Output routing | Back to LLM as tool result | Displayed to user only |

They share implementation DNA (spawning a process, capturing output, truncation) but serve different purposes. The shell escape helper can reuse utility functions from `anie-tools/src/bash.rs` or `anie-tools/src/shared.rs` if desired, but it should not go through the `Tool` trait or `ToolRegistry`.

---

## Future extensions

- `!!` — suspend TUI and run command with full terminal access (Approach B)
- `!` with no command — open an interactive subshell
- pipe agent output to a shell command: `|grep pattern`
- `/run <command>` as a slash-command alias for `!`
- configurable default shell (already handled by `BashTool`'s `$SHELL` detection)
- optional persistence of shell escape output to session (opt-in)

---

## Recommended implementation order

1. Add `UiAction::ShellEscape(String)` to the TUI
2. Add `!` prefix detection in `handle_submit`
3. Add a test that `!ls` emits `ShellEscape("ls")`
4. Add controller handling with `SystemMessage` output
5. Add a simple `run_shell_command` helper in the controller
6. Add timeout support
7. Optional: add a dedicated `ShellEscape` rendered block for nicer formatting
