# anie-rs v1.0 Milestone Checklist

This document distills the revised phase plans into a single shipping checklist.

**Release rule:**
- `v1.0` can ship when every **Release Blocker** below is complete.
- **Strongly Desired** items should land if time allows, but may slip if they are explicitly called out in release notes.
- **Post-v1.0** items are intentionally not blockers.

---

## Release Blockers

### 1. Core architecture

- [x] `AgentLoop::run(...)` uses **owned context** and returns `AgentRunResult`
- [x] `ProviderStream` yields `Result<ProviderEvent, ProviderError>`
- [x] async `RequestOptionsResolver` / `ResolvedRequestOptions` is used instead of sync key lookup
- [x] `anie-tui` stays UI-only; orchestration lives in `anie-cli`
- [x] session context preserves entry IDs (`SessionContextMessage`) for compaction/replay

### 2. Core tools

- [x] `ReadTool` works with truncation, offset/limit, and image detection
- [x] `WriteTool` works with mkdirs and per-path locking
- [x] `BashTool` works with timeout, cancellation, truncation, and process cleanup
- [x] `EditTool` works with exact replacement, overlap detection, diff output, BOM preservation, and CRLF preservation
- [x] tool arguments are validated before execution
- [x] `FileMutationQueue` canonicalizes paths before locking

### 3. Providers and local-first testing

- [x] shared HTTP/SSE infrastructure exists and is tested
- [x] OpenAI-compatible provider is implemented and tested
- [x] at least one **zero-cost local path** works end-to-end (`ollama` or `lmstudio`)
- [x] local OpenAI-compatible models work with **no API key**
- [x] manual config for custom OpenAI-compatible models works
- [x] local server auto-detection works or is intentionally disabled and documented
- [x] CLI harness works against a local provider

### 4. Interactive TUI

- [x] alternate-screen TUI launches and restores terminal correctly
- [x] user input, history, scrolling, and submission work
- [x] assistant streaming text renders in real time
- [x] tool execution blocks render correctly
- [x] edit diffs render clearly in the transcript
- [x] status bar uses provider-reported input tokens when available and falls back to estimates otherwise
- [x] `ratatui::TestBackend` snapshot tests cover key render paths
- [x] Ctrl+C aborts active work and Ctrl+D exits cleanly

### 5. Sessions and compaction

- [x] prompts are persisted immediately before each run
- [x] generated assistant/tool-result messages are persisted from `AgentRunResult`
- [x] `--resume <session_id>` resumes the most recently appended leaf in that session file
- [x] compaction preserves `first_kept_entry_id` without pointer/timestamp guessing
- [x] compaction summary generation works with the current provider stack
- [x] context overflow recovery compacts and retries
- [x] `/compact` works
- [x] `/session list` works

### 6. CLI and RPC

- [x] `anie` starts interactive mode by default
- [x] `anie "prompt"` runs print mode
- [x] `anie --rpc` speaks the **minimal versioned** JSONL protocol
- [x] RPC emits a `hello` handshake with `version: 1`
- [x] `--version` works
- [x] `--no-tools` works
- [x] onboarding prefers local providers first
- [x] onboarding hides API-key input
- [x] basic slash commands work: `/model`, `/thinking`, `/clear`, `/help`, `/compact`

### 7. Hardening and release quality

- [x] retry/backoff for transient provider errors works
- [x] structured provider errors propagate through retry and UI layers
- [x] graceful shutdown restores the terminal and flushes session state
- [x] child processes are cleaned up on cancel/exit
- [x] logs are written without exposing API keys
- [x] large `AGENTS.md` / `CLAUDE.md` files are capped by config
- [x] Linux, macOS, and Windows builds are verified
- [x] release/profile settings are validated for a reasonable binary size

---

## Strongly Desired Before v1.0

- [x] Anthropic provider ships and is tested against the real API
- [x] `/fork` is wired through the interactive flow, not just the storage layer
- [ ] session tree UX is good enough that branch behavior is understandable without reading the JSONL file
- [x] richer TUI integration tests cover event-to-render flows beyond static snapshots

---

## Optional Stretch

- [ ] Google provider
- [ ] automatic model catalog enrichment for local providers beyond `/v1/models`
- [ ] more complete RPC command surface (`compact`, richer state export, follow-ups, steering)

---

## Explicitly Post-v1.0

- [ ] GitHub Copilot OAuth
- [ ] compiled extension system (`anie-extensions`)
- [ ] `memory_write`
- [ ] explicit resume-by-leaf / branch-selection UX
- [ ] fully extensible API-kind model beyond the built-in enum
- [ ] configurable per-model thinking budget tuning (see `docs/local_model_thinking_plan.md`)

---

## Phase Sign-off Summary

- [x] **Phase 1** complete — protocol, agent loop, owned context, read/write/bash
- [x] **Phase 2** complete — OpenAI-compatible + local models, config, request resolution
- [x] **Phase 3** complete — TUI rendering + controller boundary + tests
- [x] **Phase 4** complete — session persistence + compaction + resume semantics
- [x] **Phase 5** complete — EditTool + CLI/RPC + onboarding
- [x] **Phase 6** complete — retry, shutdown, diagnostics, cross-platform validation

When all **Release Blockers** and all six **Phase Sign-off** items are checked, `anie-rs` is ready for a `v1.0` release.
