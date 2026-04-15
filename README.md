# anie

`anie` is a Rust-based coding-agent harness with an interactive terminal UI, one-shot print mode, and a JSONL RPC mode for editor or tool integrations.

The workspace is split into focused crates for protocol types, provider abstraction, built-in providers, tool execution, sessions, configuration, authentication, the TUI, and the CLI/controller.

## Highlights

- **Interactive TUI** built with `ratatui` + `crossterm`
- **Streaming output** with separate thinking and final-answer rendering when providers expose it
- **Four built-in coding tools**: `read`, `write`, `edit`, `bash`
- **Session persistence** in append-only JSONL files with fork/resume support
- **Automatic context compaction** when token budgets get tight
- **Provider support** for Anthropic, OpenAI-compatible backends, and local servers such as Ollama and LM Studio
- **Layered configuration** via `~/.anie/config.toml`, project `.anie/config.toml`, and CLI overrides
- **Credential resolution** from CLI args, `~/.anie/auth.json`, configured env vars, or built-in provider env vars
- **First-run onboarding** with local-server detection and guided config setup
- **CI coverage** for build/test and secret scanning

## Status and safety

`anie` is local coding-agent software: it can read files, write files, edit files, and run shell commands.

**Important:** tools currently run **without sandboxing or approvals**. Only use `anie` in working trees you trust it to modify.

## Quick start

### Prerequisites

- Rust `1.85` or newer
- One of:
  - an Anthropic API key
  - an OpenAI API key
  - a local OpenAI-compatible server such as Ollama or LM Studio

### Build

```bash
cargo build
```

Optional: install the binary locally:

```bash
cargo install --path crates/anie-cli
```

The installed binary name is `anie`.

### Run the interactive TUI

```bash
cargo run -p anie-cli
```

or, if installed:

```bash
anie
```

On first run, when `~/.anie/config.toml` and `~/.anie/auth.json` do not exist, `anie` will try to:

- detect a local model server
- detect common provider API key environment variables
- otherwise walk you through initial setup

### Run a one-shot prompt

```bash
cargo run -p anie-cli -- "Summarize this repository"
```

or:

```bash
anie "Summarize this repository"
```

Force print mode explicitly:

```bash
cargo run -p anie-cli -- --print "Explain crates/anie-tui/src/output.rs"
```

### Run RPC mode

```bash
cargo run -p anie-cli -- --rpc
```

### Helpful CLI options

- `--resume <session-id>` — reopen a previous session
- `-C, --cwd <dir>` — run against a different working directory
- `--model <id>` / `--provider <name>` — override model selection
- `--thinking <off|low|medium|high>` — override reasoning effort
- `--no-tools` — disable tool registration

## Configuration

`anie` loads configuration in this order:

1. built-in defaults
2. `~/.anie/config.toml`
3. the nearest project `.anie/config.toml` found by walking upward from the current working directory
4. CLI overrides such as `--model`, `--provider`, and `--thinking`

### Example: local OpenAI-compatible provider

```toml
[model]
provider = "ollama"
id = "qwen3:32b"
thinking = "medium"

[providers.ollama]
base_url = "http://localhost:11434/v1"
api = "OpenAICompletions"

[[providers.ollama.models]]
id = "qwen3:32b"
name = "Qwen 3 32B"
context_window = 32768
max_tokens = 8192
```

For custom models, you can optionally describe richer reasoning and image support with fields such as `supports_reasoning`, `reasoning_control`, `reasoning_output`, `reasoning_tag_open`, `reasoning_tag_close`, and `supports_images`.

### Project context files

`anie` can load project guidance files into the system prompt. By default it looks for:

- `AGENTS.md`
- `CLAUDE.md`

Discovery walks upward from the current working directory and is controlled by the `[context]` section in config.

## Authentication

Credentials can come from:

1. `--api-key`
2. `~/.anie/auth.json`
3. provider-specific environment variables

Custom providers can set `api_key_env` in config. Unauthenticated local servers can omit auth entirely.

Common built-in env var names include:

- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`

Saved credentials live in:

- `~/.anie/auth.json`

On Unix, `anie` writes that file with `0600` permissions.

## Usage modes

### Interactive mode

Interactive mode is the default when you run `anie` without a prompt.

Useful TUI slash commands include:

- `/model [id]`
- `/thinking [off|low|medium|high]`
- `/compact`
- `/fork`
- `/diff`
- `/session list`
- `/session <id>`
- `/tools`
- `/clear`
- `/help`
- `/quit`

### Print mode

Print mode runs a single prompt and writes the response to stdout. It is selected when:

- `--print` is passed, or
- a prompt is provided on the command line

### RPC mode

RPC mode communicates over JSONL on stdin/stdout for non-TUI integrations.

## Built-in tools

The core toolset is intentionally small and focused:

- `read` — reads text files and supported image files, with truncation controls
- `write` — writes or overwrites files, creating parent directories as needed
- `edit` — applies exact text replacements and returns diffs
- `bash` — runs shell commands in the current working directory with timeout/cancellation support

## Sessions and runtime files

`anie` stores local runtime state under `~/.anie/`.

Common files and directories include:

- `~/.anie/config.toml` — global config
- `~/.anie/auth.json` — saved API keys
- `~/.anie/state.json` — last-used non-secret runtime state
- `~/.anie/sessions/*.jsonl` — append-only session transcripts
- `~/.anie/logs/anie.log` — tracing output

You can resume a session with `anie --resume <session-id>` or switch sessions inside the TUI with `/session list` and `/session <id>`.

## Workspace layout

The workspace is split into focused crates:

- `crates/anie-cli` — CLI entry point, onboarding, controller logic, and print/RPC/interactive dispatch
- `crates/anie-tui` — terminal UI rendering and input handling
- `crates/anie-agent` — agent loop, streaming orchestration, and tool execution flow
- `crates/anie-tools` — built-in tools (`read`, `write`, `edit`, `bash`)
- `crates/anie-session` — session persistence and compaction
- `crates/anie-auth` — API key storage and request-option resolution
- `crates/anie-config` — config loading, merging, and project-context discovery
- `crates/anie-provider` — provider traits, model types, and request/response abstractions
- `crates/anie-providers-builtin` — built-in provider implementations and local-server detection
- `crates/anie-protocol` — shared protocol/message/event/content types
- `crates/anie-extensions` — placeholder crate reserved for future extension hooks

For more detail, see:

- `docs/arch/anie-rs_architecture.md`
- `docs/arch/anie-rs_build_doc.md`
- `docs/completed/phased_plan_v1-0-1/README.md`

## Development

Format:

```bash
cargo fmt --all
```

Test:

```bash
cargo test --workspace
```

Mirror the main CI build locally:

```bash
cargo build --release
cargo test --workspace
```

If you have `gitleaks` installed, you can run the secret scan locally:

```bash
gitleaks detect --config .gitleaks.toml --redact --verbose
```

## License

Licensed under either of:

- MIT
- Apache-2.0

at your option.
