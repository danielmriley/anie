//! Typed error taxonomy for the MCP client.
//!
//! Mirrors the project's "typed errors only" convention (CLAUDE.md):
//! transport and protocol failures map to `McpError` variants rather
//! than stringly-typed recovery. The manager turns these into
//! "log-and-skip-this-server" decisions; the per-tool path turns them
//! into `anie_agent::ToolError`.

/// A failure talking to an MCP server.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The server process could not be spawned (bad command, missing
    /// binary, permission denied).
    #[error("failed to spawn MCP server `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },

    /// Reading from / writing to the server's stdio failed.
    #[error("MCP transport I/O error: {0}")]
    Transport(#[from] std::io::Error),

    /// A request was not answered within the configured timeout.
    #[error("MCP request `{method}` timed out after {timeout_ms}ms")]
    Timeout { method: String, timeout_ms: u64 },

    /// The server closed its stdout before answering an in-flight
    /// request (typically a crash or clean exit).
    #[error("MCP server closed the connection before responding")]
    Closed,

    /// The server answered, but the response violated the protocol
    /// (JSON-RPC error object, missing `result`, malformed shape).
    #[error("MCP protocol error: {0}")]
    Protocol(String),

    /// The initialize/initialized handshake did not complete.
    #[error("MCP handshake failed: {0}")]
    Handshake(String),

    /// A request payload could not be serialized.
    #[error("failed to serialize MCP request: {0}")]
    Serialize(#[from] serde_json::Error),
}
