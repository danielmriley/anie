use serde::{Deserialize, Serialize};

use crate::ContentBlock;

/// A tool definition registered with the agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema describing tool parameters.
    pub parameters: serde_json::Value,
}

/// Structured output returned by a tool execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    /// Human-consumable content blocks.
    pub content: Vec<ContentBlock>,
    /// Additional structured metadata.
    pub details: serde_json::Value,
}
