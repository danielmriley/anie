//! Shared protocol types used across the anie-rs workspace.

mod content;
mod events;
mod messages;
mod stop_reason;
mod stream;
mod time;
mod tools;
mod usage;

pub use content::{ContentBlock, ToolCall};
pub use events::AgentEvent;
pub use messages::{AssistantMessage, CustomMessage, Message, ToolResultMessage, UserMessage};
pub use stop_reason::StopReason;
pub use stream::StreamDelta;
pub use time::now_millis;
pub use tools::{ToolDef, ToolResult};
pub use usage::{Cost, Usage};

#[cfg(test)]
mod tests;
