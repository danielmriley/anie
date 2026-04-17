# TUI Layout and Visual Design

## Summary

Improve visual clarity and structure of the three main TUI regions: output area,
status/info strip, and input editor.

## Current State

- User messages render as plain text with `"> You: "` prefix in cyan bold
- No visual separation between output, info, and input regions
- No top/bottom bars on the input area
- No themed user message blocks

## Action Items

### 1. Input area bars
Add styled bars above and below the input editor to visually frame it.
The top bar can host contextual info (model name, token count, cost, context
usage). The bottom bar can anchor command autocomplete and status indicators.

### 2. Region separation
Add visual separators between the three main regions:
- Scrollable output/chat area (top)
- Info strip (middle — model, context, rate limit)
- Input editor (bottom)

A thin styled line or background color shift is enough. Keep it subtle.

### 3. User message styling
Apply a background color to user message blocks instead of just a colored
prefix. Use dedicated theme tokens (`user_message_bg`, `user_message_text`)
so the treatment integrates with the theme system.

### 4. Rate limit status display
Show rate limit information in the info strip or bottom bar:
- Remaining requests / tokens in current window
- Time until reset
- Color-coded: green (healthy), yellow (approaching), red (at limit)
- Source from API response headers (`x-ratelimit-remaining`, etc.)
- Gracefully handle providers that don't return rate limit info

## Design Reference

Pi's layout:
- Scrollable output fills the top
- Widgets placed above/below editor (`setWidget`)
- Editor in the middle of the bottom section
- Footer bar across the very bottom (model, git branch, extension statuses)
- `setStatus` and `setFooter` for extensions

## Priority

Medium — improves daily usability but not blocking any features.
