# anie-rs v1.0 Milestone Checklist

This document distills the revised phase plans into a single shipping checklist.

**Release rule:**
- `v1.0` can ship when every **Release Blocker** below is complete.
- **Strongly Desired** items should land if time allows, but may slip if they are explicitly called out in release notes.
- **Post-v1.0** items are intentionally not blockers.

---

## Release Blockers

### 1. Core architecture

- [ ] `AgentLoop::run(...)` uses **owned context** and returns `AgentRunResult`
- [ ] `ProviderStream` yields `Result<ProviderEvent, ProviderError>`
- [ ] async `RequestOptionsResolver` / `ResolvedRequestOptions` is used instead of sync key lookup
- [ ] `anie-tui` stays UI-only; orchestration lives in `anie-cli`
- [ ] session context preserves entry IDs (`SessionContextMessage`) for compaction/replay

### 2. Core tools

- [ ] `ReadTool` works with truncation, offset/limit, and image detection
- [ ] `WriteTool` works with mkdirs and per-path locking
- [ ] `BashTool` works with timeout, cancellation, truncation, and process cleanup
- [ ] `EditTool` works with exact replacement, overlap detection, diff output, BOM preservation, and CRLF preservation
- [ ] tool arguments are validated before execution
- [ ] `FileMutationQueue` canonicalizes paths before locking

### 3. Providers and local-first testing

- [ ] shared HTTP/SSE infrastructure exists and is tested
- [ ] OpenAI-compatible provider is implemented and tested
- [ ] at least one **zero-cost local path** works end-to-end (`ollama` or `lmstudio`)
- [ ] local OpenAI-compatible models work with **no API key**
- [ ] manual config for custom OpenAI-compatible models works
- [ ] local server auto-detection works or is intentionally disabled and documented
- [ ] CLI harness works against a local provider

### 4. Interactive TUI

- [ ] alternate-screen TUI launches and restores terminal correctly
- [ ] user input, history, scrolling, and submission work
- [ ] assistant streaming text renders in real time
- [ ] tool execution blocks render correctly
- [ ] edit diffs render clearly in the transcript
- [ ] status bar uses provider-reported input tokens when available and falls back to estimates otherwise
- [ ] `ratatui::TestBackend` snapshot tests cover key render paths
- [ ] Ctrl+C aborts active work and Ctrl+D exits cleanly

### 5. Sessions and compaction

- [ ] prompts are persisted immediately before each run
- [ ] generated assistant/tool-result messages are persisted from `AgentRunResult`
- [ ] `--resume <session_id>` resumes the most recently appended leaf in that session file
- [ ] compaction preserves `first_kept_entry_id` without pointer/timestamp guessing
- [ ] compaction summary generation works with the current provider stack
- [ ] context overflow recovery compacts and retries
- [ ] `/compact` works
- [ ] `/session list` works

### 6. CLI and RPC

- [ ] `anie` starts interactive mode by default
- [ ] `anie "prompt"` runs print mode
- [ ] `anie --rpc` speaks the **minimal versioned** JSONL protocol
- [ ] RPC emits a `hello` handshake with `version: 1`
- [ ] `--version` works
- [ ] `--no-tools` works
- [ ] onboarding prefers local providers first
- [ ] onboarding hides API-key input
- [ ] basic slash commands work: `/model`, `/thinking`, `/clear`, `/help`, `/compact`

### 7. Hardening and release quality

- [ ] retry/backoff for transient provider errors works
- [ ] structured provider errors propagate through retry and UI layers
- [ ] graceful shutdown restores the terminal and flushes session state
- [ ] child processes are cleaned up on cancel/exit
- [ ] logs are written without exposing API keys
- [ ] large `AGENTS.md` / `CLAUDE.md` files are capped by config
- [ ] Linux, macOS, and Windows builds are verified
- [ ] release/profile settings are validated for a reasonable binary size

---

## Strongly Desired Before v1.0

- [ ] Anthropic provider ships and is tested against the real API
- [ ] `/fork` is wired through the interactive flow, not just the storage layer
- [ ] session tree UX is good enough that branch behavior is understandable without reading the JSONL file
- [ ] richer TUI integration tests cover event-to-render flows beyond static snapshots

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

- [ ] **Phase 1** complete — protocol, agent loop, owned context, read/write/bash
- [ ] **Phase 2** complete — OpenAI-compatible + local models, config, request resolution
- [ ] **Phase 3** complete — TUI rendering + controller boundary + tests
- [ ] **Phase 4** complete — session persistence + compaction + resume semantics
- [ ] **Phase 5** complete — EditTool + CLI/RPC + onboarding
- [ ] **Phase 6** complete — retry, shutdown, diagnostics, cross-platform validation

When all **Release Blockers** and all six **Phase Sign-off** items are checked, `anie-rs` is ready for a `v1.0` release.