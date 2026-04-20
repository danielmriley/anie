# Commands and Slash Menu

## Summary

Expand the built-in command set and add an inline autocomplete dropdown
when the user types `/` in the input editor.

## Current State

Anie now has graceful slash-command dispatch and an inline
autocomplete popup. Typing `/` opens a filterable palette of
registered commands (builtins + future extension/prompt/skill
entries); typing a space after a command with an `Enumerated` or
`Subcommands` argument spec shows the valid values. Validation
against `SlashCommandInfo::validate` happens before the TUI
dispatches a `UiAction`, so a bad argument never reaches the
controller.

Commands shipped today: `/model`, `/thinking`, `/compact`,
`/fork`, `/diff`, `/new`, `/session`, `/tools`, `/onboard`,
`/providers`, `/clear`, `/reload`, `/copy`, `/help`, `/quit`.

Toggle the popup via `ui.slash_command_popup_enabled = false` in
`~/.anie/config.toml`; validation still runs when the popup is
disabled.

## Action Items

### 1. Inline command autocomplete menu — **done** (plan 12)
Popup opens on `/` at line start; filters in real time; arrow
keys + Enter/Tab + Escape; sourced from `SlashCommandInfo`
catalog so extensions, prompts, and skills get the popup for
free once they land. Max visible items defaults to 5.

### 2. Additional built-in commands to consider

| Command | Description | Status |
|---------|-------------|--------|
| `/settings` | View/modify configuration interactively | Not implemented |
| `/resume` | Browse and select a previous session | Not implemented |
| `/new` | Start a fresh session | Not implemented |
| `/name <name>` | Set session display name | Not implemented |
| `/session` | Show session info (path, tokens, cost) | Not implemented |
| `/tree` | Navigate session history tree | Not implemented |
| `/fork` | Branch from a point in history | Not implemented |
| `/copy` | Copy last assistant response to clipboard | Not implemented |
| `/export [file]` | Export session to HTML | Not implemented |
| `/reload` | Hot-reload config, context files, etc. | Not implemented |
| `/compact` | Manually trigger context compaction | Not implemented |
| `/quit` | Quit anie | Not implemented |

### 3. `/settings` command
Interactive settings viewer/editor:
- Model & provider configuration
- Thinking level
- Theme preferences
- Compaction settings
- Store in `~/.anie/settings.json` (global) and `.anie/settings.json` (project)
- Project settings override global

### 4. `/copy` command
Copy the last assistant response text to the system clipboard.
Straightforward utility command.

## Priority

Medium — `/settings`, `/copy`, `/new`, `/resume` are the most useful near-term.
The autocomplete menu is a quality-of-life improvement that becomes more
valuable as the command set grows.
