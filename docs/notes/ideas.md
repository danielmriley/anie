# Project Ideas & Notes

Ideas and planned improvements, organized by topic. Each file contains
a summary, current state, action items, and priority.

## Active Bug

- [Thinking Block Display Bug](thinking_block_display_bug.md) — thinking text sometimes visible at end of messages (High)

## Core Functionality

- [Local Model Support](local_model_support.md) — context length detection, automatic compaction, parallel tool calling
- [Context File Handling](context_file_handling.md) — make AGENTS.md reactive to mid-session edits
- [Skills System](skills_system.md) — Agent Skills standard support with `/skill:name` commands

## TUI

- [TUI Layout and Visual Design](tui_layout_and_visual_design.md) — input bars, region separation, user message styling, rate limit display
- [Commands and Slash Menu](commands_and_slash_menu.md) — expanded command set, inline autocomplete, `/settings`, `/copy`

## Providers and Auth

- [Provider Expansion and Auth](provider_expansion_and_auth.md) — additional providers, OAuth/subscription support

## Tools

- [Internet Search Tool](internet_search_tool.md) — self-hosted search via SearXNG

## Long-Term

- [Memory System](memory_system.md) — persistent graph-based memory across sessions
- [Daemon and Messaging](daemon_and_messaging.md) — background daemon, Telegram/Discord integrations
- [Benchmarks and Evaluation](benchmarks_and_evaluation.md) — internal benchmark suite, TerminalBench investigation

## Completed / Addressed

- ~~Thinking Levels for Local Models~~ → addressed in [`../completed/reasoning_fix_plan.md`](../completed/reasoning_fix_plan.md) (phases 1–3)
- ~~Onboarding Enhancement~~ → implemented; see [`../completed/onboarding_plans/`](../completed/onboarding_plans/)

## Design Documents (not yet implemented)

- [Compat System Plan](../compat_system_plan.md) — per-model/per-provider OpenAI-compatible backend flags (set aside for now)
