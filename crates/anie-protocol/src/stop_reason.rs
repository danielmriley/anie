use serde::{Deserialize, Serialize};

/// Canonical reasons an assistant turn may stop.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum StopReason {
    /// The provider finished naturally.
    Stop,
    /// The assistant requested tool execution.
    ToolUse,
    /// The provider or agent encountered an error.
    Error,
    /// The run was aborted by cancellation.
    Aborted,
}
