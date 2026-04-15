use crate::{AssistantMessage, Message, StreamDelta, ToolResult, ToolResultMessage};

/// In-process events emitted by the agent loop.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// The agent run has started.
    AgentStart,
    /// The agent run has ended.
    AgentEnd { messages: Vec<Message> },
    /// A turn has started.
    TurnStart,
    /// A turn has ended.
    TurnEnd {
        assistant: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },
    /// A message has started.
    MessageStart { message: Message },
    /// A message delta was received.
    MessageDelta { delta: StreamDelta },
    /// A message has ended.
    MessageEnd { message: Message },
    /// A tool execution has started.
    ToolExecStart {
        call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// A tool execution emitted a partial update.
    ToolExecUpdate {
        call_id: String,
        partial: ToolResult,
    },
    /// A tool execution has finished.
    ToolExecEnd {
        call_id: String,
        result: ToolResult,
        is_error: bool,
    },
    /// Replace the rendered transcript with reconstructed history.
    TranscriptReplace { messages: Vec<Message> },
    /// A neutral controller-originated message for transcript display.
    SystemMessage { text: String },
    /// Status-bar state changed outside provider-stream events.
    StatusUpdate {
        provider: String,
        model_name: String,
        thinking: String,
        estimated_context_tokens: u64,
        context_window: u64,
        cwd: String,
        session_id: String,
    },
    /// Context compaction has started.
    CompactionStart,
    /// Context compaction completed successfully.
    CompactionEnd {
        summary: String,
        tokens_before: u64,
        tokens_after: u64,
    },
    /// A transient provider failure has been scheduled for retry.
    RetryScheduled {
        attempt: u32,
        max_retries: u32,
        delay_ms: u64,
        error: String,
    },
}
