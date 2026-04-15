use crate::ToolCall;

/// Incremental updates emitted while an assistant message is streaming.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    /// A text block started.
    TextStart,
    /// Text content delta.
    TextDelta(String),
    /// A text block ended.
    TextEnd,
    /// A thinking block started.
    ThinkingStart,
    /// Thinking content delta.
    ThinkingDelta(String),
    /// A thinking block ended.
    ThinkingEnd,
    /// A tool call started.
    ToolCallStart(ToolCall),
    /// A tool call argument fragment arrived.
    ToolCallDelta { id: String, arguments_delta: String },
    /// A tool call ended.
    ToolCallEnd { id: String },
}
