# anie

`anie` is a Rust-based coding-agent harness with an interactive terminal UI, one-shot print mode, and a JSONL RPC mode for editor or tool integrations.

The workspace is split into focused crates for protocol types, provider abstraction, built-in providers, tool execution, sessions, configuration, authentication, the TUI, and the CLI/controller.

## Highlights

- **Interactive TUI** built with `ratatui` + `crossterm`
- **Streaming output** with separate thinking and final-answer rendering when providers expose it
- **Built-in coding tools** for files, shell, search, and web access: `read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`, plus `web_read` and `web_search` in the default build
- **Session persistence** in append-only JSONL files with fork/resume support
- **Automatic context compaction** when token budgets get tight
- **Provider support** for Anthropic Messages, OpenAI-compatible Chat Completions, native Ollama `/api/chat`, OpenRouter routing, and local servers such as Ollama and LM Studio
- **Dynamic model pickers** for onboarding, `/model`, and provider browsing, with search and refresh
- **Layered configuration** via `~/.anie/config.toml`, project `.anie/config.toml`, runtime `~/.anie/state.json`, and CLI overrides
- **Credential resolution** from CLI args, the OS keyring, JSON fallback files, configured env vars, or built-in provider env vars
- **First-run onboarding** with a full-screen TUI, local-server detection, provider presets, and provider management overlays
- **CI coverage** for build/test, formatting, clippy, MSRV checks, and secret scanning

## Architecture summary

`anie` is organized around a small set of explicit boundaries:

- `anie-cli` owns CLI parsing, mode dispatch, onboarding commands, controller orchestration, retry policy, slash-command registration, and runtime-state persistence.
- `anie-tui` owns terminal UI state, active-draft input, overlays, transcript rendering, autocomplete, and slash-command pre-dispatch validation; it sends user intent to the controller as `UiAction` values and renders `AgentEvent` updates.
- `anie-agent` owns the provider/tool-agnostic agent loop. It receives owned prompt/context state, streams provider events, validates and executes tool calls, and returns generated messages to the caller for persistence.
- `anie-provider` defines the provider contract, model metadata, request options, normalized streaming events, native Ollama `num_ctx` override plumbing, and the typed `ProviderError` taxonomy used for retry decisions.
- `anie-providers-builtin` implements the registered provider backends: Anthropic Messages, OpenAI-compatible Chat Completions, and native Ollama `/api/chat`. OpenRouter, LM Studio, and custom OpenAI-compatible endpoints are routed through config/model/base-url/compat behavior on top of those shapes.
- `anie-tools` implements the core file/shell/search tools. `anie-tools-web` implements the default web tools behind the `web` cargo feature. `write` and `edit` share a file-mutation queue so file writes are serialized.
- `anie-session`, `anie-config`, and `anie-auth` own persistence boundaries: append-only session JSONL, layered TOML config, runtime state, keyring/JSON credentials, and OAuth refresh locking.

The canonical design reference is [`docs/arch/anie-rs_architecture.md`](docs/arch/anie-rs_architecture.md). Update it alongside architecture-significant changes; it is intended to be the source of truth for crate ownership, runtime flow, persistence formats, provider/tool contracts, hot paths, and known refactor risks.

## Status and safety

`anie` is local coding-agent software: it can read files, write files, edit files, search directories, list directories, and run shell commands.

**Important:** tools currently run **without sandboxing or approvals**. This is intentional for now: relative paths resolve from the session cwd, but absolute paths and `..` traversal are allowed, and shell commands have the same system access as the `anie` process. Only use `anie` in environments you trust it to access. Future work may add WASM/containerized tool execution for stronger isolation.

## Quick start

### Prerequisites

- Rust `1.85` or newer
- One of:
  - an Anthropic API key
  - an OpenAI API key
  - a local server such as Ollama or LM Studio

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

On first run, `anie` launches a full-screen onboarding UI when no provider config or saved credentials are available.

The onboarding flow can:

- detect a local model server such as Ollama or LM Studio
- let you add an API-key-backed provider from a preset list
- let you add a custom OpenAI-compatible endpoint
- discover available models for each provider path and let you pick one inline
- reopen later with `anie onboard` or `/onboard`

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

You can rerun the onboarding flow anytime with:

```bash
anie onboard
```

List available models from the CLI with:

```bash
anie models [--provider <name>] [--refresh]
```

- `--resume <session-id>` — reopen a previous session
- `-C, --cwd <dir>` — run against a different working directory
- `--model <id>` / `--provider <name>` — override model selection
- `--thinking <off|minimal|low|medium|high>` — override reasoning effort
- `--no-tools` — disable tool registration

## Configuration

`anie` loads configuration in this order:

1. built-in defaults
2. `~/.anie/config.toml`
3. the nearest project `.anie/config.toml` found by walking upward from the current working directory
4. CLI overrides such as `--model`, `--provider`, and `--thinking`

### Example: native Ollama provider

```toml
[model]
provider = "ollama"
id = "qwen3.5:0.8b"
thinking = "Off"

[providers.ollama]
base_url = "http://localhost:11434"
api = "OllamaChatApi"

[[providers.ollama.models]]
id = "qwen3.5:0.8b"
name = "Qwen 3.5 0.8B"
context_window = 32768
max_tokens = 2048
```

For custom models, you can optionally describe richer reasoning and image support with fields such as `supports_reasoning`, `reasoning_control`, `reasoning_output`, `reasoning_tag_open`, `reasoning_tag_close`, and `supports_images`.

Native Ollama models use Ollama's root URL and can honor `think` and `options.num_ctx`. Legacy OpenAI-compatible Ollama configs using `/v1` still work, but that path cannot apply native `/context-length` overrides.

You can set a workspace-wide cap for discovered Ollama context windows:

```toml
[ollama]
default_max_num_ctx = 32768
```

This cap is applied to native Ollama models at catalog-load time. A per-model runtime override from `/context-length <N>` takes precedence until reset.

### Bash deny policy

You can configure a pre-spawn bash deny policy as an accidental-risk guardrail:

```toml
[tools.bash.policy]
enabled = true
deny_commands = ["rm", "dd", "mkfs"]
deny_patterns = [
  'git\s+push\s+--force',
  'curl\b.*\|\s*(sh|bash)',
]
```

`deny_commands` matches simple command names and basenames such as `rm` or `/bin/rm`. `deny_patterns` are regular expressions matched against the raw command string before the shell is spawned.

This policy is **not a sandbox** and should not be treated as a security boundary. Shell indirection, scripts, interpreters, and non-bash tools can bypass textual checks. It is meant to reduce accidental execution of commands a user never wants anie to run.

### Web tool policy

The default build includes `web_read` and `web_search`. Their fetch behavior is controlled by `[tools.web]`:

```toml
[tools.web]
request_timeout_secs = 30
headless_timeout_secs = 30
max_page_bytes = 10485760
max_redirects = 10
allow_private_ips = false
```

The default non-headless fetch path rejects private, loopback, link-local, and redirected private destinations before issuing requests, enforces page/error/robots byte caps, observes cancellation, and rate-limits per host. `web_read(javascript=true)` requires the `web-headless` feature plus Chrome/Chromium; that path is useful for JavaScript-heavy pages but has a weaker network boundary because Chrome owns subresource and redirect fetching.

### Project context files

`anie` can load project guidance files into the system prompt. By default it looks for:

- `AGENTS.md`
- `CLAUDE.md`

Discovery walks upward from the current working directory and is controlled by the `[context]` section in config.

## Authentication

Credentials resolve in this order:

1. `--api-key`
2. OS-native keyring storage via `CredentialStore`
3. JSON compatibility files (`~/.anie/auth.json`, then `~/.anie/auth.json.migrated`)
4. provider-specific environment variables

Custom providers can set `api_key_env` in config. Unauthenticated local servers can omit auth entirely.

Common built-in env var names include:

- `ANTHROPIC_API_KEY`
- `OPENAI_API_KEY`

Saved credentials are written to your operating system's credential store when native keyring support is available:

- macOS: Keychain
- Windows: Credential Manager
- Linux: kernel keyring backend in the current build

The current implementation also mirrors credentials into `~/.anie/auth.json` as a compatibility store so provider enumeration, headless fallback, and older flows continue to work. Legacy plaintext credentials are migrated to the keyring on startup and preserved as `~/.anie/auth.json.migrated`.

## Usage modes

### Interactive mode

Interactive mode is the default when you run `anie` without a prompt.

Useful TUI slash commands include:

- `/model [query]` — open the model picker, or switch immediately on an exact match
- `/thinking [off|minimal|low|medium|high]`
- `/context-length [N|reset]` — query or override native Ollama `num_ctx` for the current model
- `/state` — show current model, context-window layers, session id, and persistent file paths
- `/compact`
- `/fork`
- `/diff`
- `/new`
- `/session list`
- `/session <id>`
- `/tools`
- `/onboard`
- `/providers`
- `/reload`
- `/copy`
- `/markdown [on|off]`
- `/tool-output [verbose|compact]`
- `/login <provider>`
- `/logout <provider>`
- `/clear`
- `/help`
- `/quit`

Useful keyboard shortcuts include:

- `Ctrl+O` — open the model picker

### Print mode

Print mode runs a single prompt and writes the response to stdout. It is selected when:

- `--print` is passed, or
- a prompt is provided on the command line

### RPC mode

RPC mode communicates over JSONL on stdin/stdout for non-TUI integrations.

## Built-in tools

The default toolset is intentionally small and focused:

- `read` — reads text files and supported image files, with truncation controls
- `write` — writes or overwrites files, creating parent directories as needed
- `edit` — applies exact text replacements and returns diffs
- `bash` — runs shell commands in the current working directory with timeout/cancellation support
- `grep` — searches file contents
- `find` — finds files by name/pattern
- `ls` — lists files and directories
- `web_read` — fetches a web page and returns extracted Markdown, when the `web` feature is enabled
- `web_search` — searches the live web and returns ranked URLs/snippets, when the `web` feature is enabled

## Sessions and runtime files

`anie` stores local runtime state under `~/.anie/`.

Common files and directories include:

- `~/.anie/config.toml` — global config
- `./.anie/config.toml` — nearest project config when present
- `~/.anie/auth.json` — JSON credential fallback when native keyring storage is unavailable
- `~/.anie/auth.json.migrated` — preserved backup after legacy credential migration
- `~/.anie/state.json` — last-used non-secret runtime state, including per-model native Ollama context-length overrides
- `~/.anie/sessions/*.jsonl` — append-only session transcripts
- `~/.anie/logs/anie.log` — tracing output

You can resume a session with `anie --resume <session-id>` or switch sessions inside the TUI with `/session list` and `/session <id>`.

anie takes an exclusive advisory lock on the session file for the
lifetime of the open session. If you try to `--resume` a session
another `anie` process currently has open, the second process
exits with an error pointing you to `/fork` or a new session. On
filesystems that don't support advisory locks (some network
filesystems), the lock attempt is a no-op and a warning is
logged.

## Workspace layout

The workspace is split into focused crates:

- `crates/anie-cli` — CLI entry point, onboarding, controller logic, and print/RPC/interactive dispatch
- `crates/anie-tui` — terminal UI rendering and input handling
- `crates/anie-agent` — agent loop, streaming orchestration, and tool execution flow
- `crates/anie-tools` — built-in tools (`read`, `write`, `edit`, `bash`, `grep`, `find`, `ls`)
- `crates/anie-tools-web` — optional/default web tools (`web_read`, `web_search`) and web fetch/search safety boundaries
- `crates/anie-session` — session persistence and compaction
- `crates/anie-auth` — API key storage and request-option resolution
- `crates/anie-config` — config loading, merging, and project-context discovery
- `crates/anie-provider` — provider traits, model types, and request/response abstractions
- `crates/anie-providers-builtin` — built-in provider implementations and local-server detection
- `crates/anie-protocol` — shared protocol/message/event/content types

Extensions are designed as a future out-of-process plugin system;
see `docs/refactor_plans/10_extension_system_pi_port.md`.

For more detail, see:

- `docs/README.md` — docs tree entry point
- `docs/arch/anie-rs_architecture.md`
- `docs/arch/credential_resolution.md`
- `docs/arch/onboarding_flow.md`
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

Clippy:

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

Mirror the main CI build locally:

```bash
cargo build --release
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo +1.85 check --workspace --all-targets
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
