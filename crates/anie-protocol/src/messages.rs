use serde::{Deserialize, Serialize};

use crate::{ContentBlock, StopReason, Usage};

/// A conversation message preserved across providers, sessions, and UI rendering.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role")]
pub enum Message {
    /// A user-authored message.
    #[serde(rename = "user")]
    User(UserMessage),
    /// A model-authored assistant message.
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    /// A tool result fed back into the conversation.
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
    /// An opaque custom message preserved by the core system.
    #[serde(rename = "custom")]
    Custom(CustomMessage),
}

/// A user message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserMessage {
    /// Structured user content.
    pub content: Vec<ContentBlock>,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
}

/// An assistant message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssistantMessage {
    /// Structured assistant content.
    pub content: Vec<ContentBlock>,
    /// Usage reported by the provider, if available.
    pub usage: Usage,
    /// Why generation ended.
    pub stop_reason: StopReason,
    /// Optional human-readable error text for transcript display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Provider identifier.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
    /// Provider-emitted reasoning artifacts that must be replayed
    /// verbatim on the next turn to preserve reasoning context.
    /// Currently populated only by OpenRouter for upstream models
    /// that flag `supports_reasoning_details_replay` (openai/o*,
    /// openai/gpt-5*). The payload is stored as opaque JSON so
    /// schema changes from the upstream don't require a migration.
    /// `None` on every other provider; default on load for
    /// forward-compat with older session files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<serde_json::Value>>,
}

/// A tool result message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResultMessage {
    /// Tool call identifier being satisfied.
    pub tool_call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Structured tool output.
    pub content: Vec<ContentBlock>,
    /// Additional structured metadata from the tool.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    /// Whether the tool execution failed.
    pub is_error: bool,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
}

/// An extension-defined custom message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomMessage {
    /// Custom message discriminator.
    pub custom_type: String,
    /// Opaque custom payload.
    pub content: serde_json::Value,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
}
