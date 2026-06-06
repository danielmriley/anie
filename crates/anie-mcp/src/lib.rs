//! MCP (Model Context Protocol) client for anie.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]
//!
//! anie connects to external MCP servers as a *client*: it spawns
//! each configured server over stdio, completes the JSON-RPC
//! handshake, discovers the server's tools, and registers each as a
//! dynamic [`anie_agent::Tool`] in the existing tool registry. Scope
//! is **client + tools only** — resources, prompts, SSE/HTTP
//! transport, OAuth bridging, and exposing anie *as* a server are
//! out of scope. See `docs/mcp_client/README.md`.
//!
//! The transport is a hand-rolled JSON-RPC-2.0-over-stdio client; no
//! external MCP SDK is pulled in (see the plan's dependency
//! rationale). The only public seam consumers depend on is
//! `Arc<dyn Tool>` produced by the manager, so swapping the
//! transport internals later (e.g. for SSE) touches nothing
//! downstream.

mod client;
mod error;
mod protocol;
mod transport;

pub use client::McpClient;
pub use error::McpError;
pub use protocol::{CallToolResult, ListToolsResult, McpContent, McpToolSpec, ResourceRef};
pub use transport::StdioTransport;
