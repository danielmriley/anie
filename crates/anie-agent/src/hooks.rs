//! Tool-execution hook traits used by the agent loop.
//!
//! These traits are `pub(crate)` by design. Today they are consumed
//! only inside `anie-agent`. A public extension API is planned
//! separately as an out-of-process JSON-RPC system; see
//! `docs/refactor_plans/10_extension_system_pi_port.md`.
//!
//! If another crate needs to participate in tool hooks, update plan 10
//! first — the JSON-RPC host is the supported path.

#![cfg_attr(not(test), allow(dead_code))]

use async_trait::async_trait;

use anie_protocol::{ContentBlock, Message, ToolCall, ToolResult};

/// Result of a before-tool-call hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeforeToolCallResult {
    /// Allow tool execution to proceed.
    Allow,
    /// Block tool execution and surface the reason back to the model.
    Block { reason: String },
}

/// Optional override applied after a tool finishes.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ToolResultOverride {
    /// Replacement content blocks.
    pub(crate) content: Option<Vec<ContentBlock>>,
    /// Replacement details payload.
    pub(crate) details: Option<serde_json::Value>,
    /// Replacement error flag.
    pub(crate) is_error: Option<bool>,
}

/// Hook invoked before a tool executes.
#[async_trait]
pub(crate) trait BeforeToolCallHook: Send + Sync {
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
pub(crate) trait AfterToolCallHook: Send + Sync {
    /// Optionally override a completed tool result.
    async fn after_tool_call(
        &self,
        tool_call: &ToolCall,
        result: &ToolResult,
        is_error: bool,
    ) -> Option<ToolResultOverride>;
}

#[cfg(test)]
mod tests {
    use super::BeforeToolCallResult;

    #[test]
    fn allow_variant_is_constructible() {
        let result = BeforeToolCallResult::Allow;
        assert!(matches!(result, BeforeToolCallResult::Allow));
    }
}
