use serde::{Deserialize, Serialize};

/// A structured content block carried by user, assistant, and tool-result messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain UTF-8 text.
    #[serde(rename = "text")]
    Text { text: String },
    /// Base64-encoded image data.
    #[serde(rename = "image")]
    Image { media_type: String, data: String },
    /// Model thinking / reasoning content.
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    /// A structured tool invocation proposed by the assistant.
    #[serde(rename = "toolCall")]
    ToolCall(ToolCall),
}

/// A tool call emitted by the assistant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    /// Provider-supplied call identifier.
    pub id: String,
    /// Registered tool name.
    pub name: String,
    /// Parsed JSON arguments.
    pub arguments: serde_json::Value,
}
