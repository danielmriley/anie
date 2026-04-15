use async_trait::async_trait;

use anie_protocol::{ContentBlock, Message, ToolCall, ToolResult};

/// Result of a before-tool-call hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeToolCallResult {
    /// Allow tool execution to proceed.
    Allow,
    /// Block tool execution and surface the reason back to the model.
    Block { reason: String },
}

/// Optional override applied after a tool finishes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResultOverride {
    /// Replacement content blocks.
    pub content: Option<Vec<ContentBlock>>,
    /// Replacement details payload.
    pub details: Option<serde_json::Value>,
    /// Replacement error flag.
    pub is_error: Option<bool>,
}

/// Hook invoked before a tool executes.
#[async_trait]
pub trait BeforeToolCallHook: Send + Sync {
    /// Inspect a pending tool call and optionally block it.
    async fn before_tool_call(
        &self,
        tool_call: &ToolCall,
        args: &serde_json::Value,
        context: &[Message],
    ) -> BeforeToolCallResult;
}

/// Hook invoked after a tool executes.
#[async_trait]
pub trait AfterToolCallHook: Send + Sync {
    /// Optionally override a completed tool result.
    async fn after_tool_call(
        &self,
        tool_call: &ToolCall,
        result: &ToolResult,
        is_error: bool,
    ) -> Option<ToolResultOverride>;
}
