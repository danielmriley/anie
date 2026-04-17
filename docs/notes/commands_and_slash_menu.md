# Commands and Slash Menu

## Summary

Expand the built-in command set and add an inline autocomplete dropdown
when the user types `/` in the input editor.

## Current State

Anie has some slash commands (`/thinking`, `/model`, `/providers`, `/onboard`,
`/help`). There is no autocomplete menu — the user must know the command name.

## Action Items

### 1. Inline command autocomplete menu
When the user types `/`, show a dropdown anchored near the input area:
- Filter in real-time as the user types
- Show command name + short description
- Keyboard navigable (arrows, Enter, Escape)
- Include all registered commands (built-in, future skills, prompt templates)
- Configurable max visible items

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
