//! Minimal serde types for the MCP JSON-RPC methods anie uses:
//! `initialize`, `tools/list`, and `tools/call`. Only the fields the
//! client reads are modelled; serde ignores the rest, so servers that
//! send extra fields (most do) deserialize cleanly. Resources,
//! prompts, sampling, and progress are out of scope (deferred).

use serde::Deserialize;
use serde_json::Value;

/// The MCP protocol revision anie's client implements and advertises
/// in `initialize`. Servers that require a newer revision fail the
/// handshake and are skipped (logged) rather than crashing anie.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Result of `tools/list`.
#[derive(Debug, Clone, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<McpToolSpec>,
    /// Opaque pagination cursor. When present, the client must re-issue
    /// `tools/list` with `{ "cursor": <nextCursor> }` to fetch the next
    /// page until it is absent.
    #[serde(rename = "nextCursor", default)]
    pub next_cursor: Option<String>,
}

/// One tool advertised by an MCP server. `input_schema` is a JSON
/// Schema object — the exact shape anie's `ToolDef.parameters` holds.
#[derive(Debug, Clone, Deserialize)]
pub struct McpToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments. Defaults to an empty
    /// object if the server omits it.
    #[serde(rename = "inputSchema", default = "empty_object")]
    pub input_schema: Value,
}

/// Result of `tools/call`.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    /// True when the tool reported an error. Absent means success.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// A single content block in a `tools/call` result. Unknown block
/// types (audio, etc.) deserialize to [`McpContent::Unknown`] rather
/// than failing the whole call.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: ResourceRef,
    },
    #[serde(other)]
    Unknown,
}

/// Embedded resource reference inside a `resource` content block.
/// Only the fields anie surfaces are modelled.
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceRef {
    pub uri: String,
    #[serde(default)]
    pub text: Option<String>,
}
