# Anie Roadmap

Unified, prioritized task list. Items are ordered by impact-to-effort
ratio — smallest impactful changes first. Check off items as they ship.

## Completed

- [x] Fix reasoning-only completion bug (reasoning_fix_plan.md phases 1–3)
- [x] Thinking block display regression tests (7 tests added)
- [x] Dynamic model discovery and picker menus
- [x] Onboarding flow

## Next Up — Small, High-Impact

### 1. Context file hot-reload
**What**: Re-read AGENTS.md / CLAUDE.md before each LLM turn if changed.
**Why**: Common papercut — users edit context files and changes don't take
effect until restart.
**Effort**: Small — stat-check mtime, rebuild system prompt if changed.
**Details**: [docs/notes/context_file_handling.md](notes/context_file_handling.md)

### 2. `/copy` command
**What**: Copy the last assistant response to the system clipboard.
**Why**: Frequently wanted, trivial to implement.
**Effort**: Tiny — one command handler + clipboard crate.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

### 3. `/new` command
**What**: Start a fresh session without restarting anie.
**Why**: Currently requires quitting and relaunching.
**Effort**: Small — reset session state, clear transcript.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

### 4. `/reload` command
**What**: Hot-reload config, context files, and keybindings.
**Why**: Enables mid-session config changes. Also makes context file
hot-reload available on demand (complements item 1).
**Effort**: Small — re-run config loading, rebuild system prompt.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

## Medium-Term — Core Functionality

### 5. Automatic context compaction
**What**: Trigger compaction automatically when approaching the context limit.
**Why**: Prevents context overflow errors. Currently compaction exists but
must be triggered manually or by overflow recovery.
**Effort**: Medium — threshold detection, automatic trigger, TUI indicator.
**Details**: [docs/notes/local_model_support.md](notes/local_model_support.md)

### 6. Local model context length detection
**What**: Query Ollama/vLLM for actual context window size instead of
defaulting to 32K.
**Why**: Incorrect context length leads to either wasted capacity or
overflow errors.
**Effort**: Medium — API queries, config override, caching.
**Details**: [docs/notes/local_model_support.md](notes/local_model_support.md)

### 7. Slash command autocomplete menu
**What**: Show a filterable dropdown when the user types `/`.
**Why**: Discoverability — users shouldn't need to memorize commands.
**Effort**: Medium — TUI widget, real-time filtering, keyboard navigation.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

### 8. Session management commands (`/resume`, `/session`, `/name`)
**What**: Browse past sessions, show session info, set display names.
**Why**: Session management currently requires CLI flags or filesystem
knowledge.
**Effort**: Medium — session listing UI, metadata display.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

## Longer-Term — Features

### 9. TUI layout improvements
**What**: Input area bars, region separation, user message styling.
**Why**: Visual clarity — the three TUI regions blur together.
**Effort**: Medium-large — layout restructuring, theme tokens.
**Details**: [docs/notes/tui_layout_and_visual_design.md](notes/tui_layout_and_visual_design.md)

### 10. Skills system
**What**: Agent Skills standard support — load SKILL.md files, register
as `/skill:name` commands.
**Why**: Enables repeatable, project-specific agent behaviors.
**Effort**: Medium — file loading, frontmatter parsing, command registration.
**Details**: [docs/notes/skills_system.md](notes/skills_system.md)

### 11. `/settings` command
**What**: Interactive settings viewer/editor in the TUI.
**Why**: Currently all config changes require editing TOML files.
**Effort**: Medium-large — TUI overlay, config mutation, persistence.
**Details**: [docs/notes/commands_and_slash_menu.md](notes/commands_and_slash_menu.md)

### 12. Provider expansion
**What**: Additional provider presets (Google Gemini, Mistral, Groq, etc.).
**Why**: Broader model access without manual config.
**Effort**: Medium per provider — most are OpenAI-compatible.
**Details**: [docs/notes/provider_expansion_and_auth.md](notes/provider_expansion_and_auth.md)

## Long-Term — Architecture

### 13. Internet search tool
**What**: Self-hosted search via SearXNG + page content extraction.
**Details**: [docs/notes/internet_search_tool.md](notes/internet_search_tool.md)

### 14. Memory system
**What**: Persistent graph-based memory across sessions.
**Details**: [docs/notes/memory_system.md](notes/memory_system.md)

### 15. Daemon and messaging integrations
**What**: Background daemon with Telegram/Discord frontends.
**Details**: [docs/notes/daemon_and_messaging.md](notes/daemon_and_messaging.md)

### 16. Benchmarks and evaluation
**What**: Internal benchmark suite, TerminalBench investigation.
**Details**: [docs/notes/benchmarks_and_evaluation.md](notes/benchmarks_and_evaluation.md)

## Refactors (do opportunistically)

Detailed plans in [docs/refactor_plans/](refactor_plans/README.md).
Pick items when they unblock feature work or reduce pain.

| # | Refactor | When to do it |
|---|----------|---------------|
| 00 | CI enforcement | Anytime (10 min) |
| 01 | openai.rs module split | Before next provider work |
| 02 | TUI overlay trait | Before adding more overlays |
| 03 | Controller decomposition | Before command system grows |
| 06 | Session write locking | Before daemon work |
| 08 | Small hygiene items | Ongoing |

## Design Documents (parked)

- [Compat system plan](compat_system_plan.md) — per-model backend flags.
  Parked until real local model problems drive the design.
- [Reasoning fix plan](reasoning_fix_plan.md) — phases 1–3 implemented.
  No further phases planned.
