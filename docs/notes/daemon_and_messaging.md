# Persistent Daemon and Messaging Integrations

## Summary

Run anie as a persistent background daemon that maintains state, memory,
and session continuity. Multiple frontends (TUI, Telegram, Discord, etc.)
connect to the same running agent.

## Current State

Anie runs as a foreground TUI process. Each invocation is independent.
There is no daemon mode or messaging integration.

## Action Items

### 1. Daemon architecture
- Single agent process that runs in the background
- Maintains active session, memory, and configuration state
- IPC mechanism: Unix socket, local HTTP/WebSocket, or named pipe
- Authentication so only trusted clients can connect
- Lifecycle management: start on login, crash recovery, graceful shutdown
- Headless mode (no TUI) for pure messaging-driven use

### 2. TUI as a client
The current TUI becomes one client to the daemon, not the only entry
point. When the TUI connects, it attaches to the daemon's active session
and renders the conversation history.

### 3. Messaging integrations
Each integration maps its message format to anie's standard input/output:

| Platform | Priority | Notes |
|----------|----------|-------|
| Telegram | High (user interest) | Send/receive messages, file attachments, inline replies |
| Discord | Low | Bot API, channel-based interaction |
| Slack | Low | Workspace integration |
| SMS (Twilio) | Low | Simple text-only interface |
| Email | Low | Async, longer-form interactions |

### 4. Session continuity
- Daemon keeps the session warm between interactions
- Pairs with the memory system — daemon keeps memory loaded
- Messages from any frontend continue the same conversation
- Consider multi-session support (different sessions for different
  frontends or contexts)

## Design Considerations

- The daemon needs to handle concurrent requests from multiple clients
- Rate limiting per client to prevent abuse
- Graceful handling of disconnects and reconnects
- Configuration for which integrations are active

## Priority

Low — long-term architectural goal. The Telegram integration has been
mentioned as a near-term interest, but the full daemon architecture
is a larger effort. Consider a simpler Telegram bot first (direct
process, no daemon) as a stepping stone.
