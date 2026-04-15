//! Core agent loop, tool contracts, and execution hooks for anie-rs.

mod agent_loop;
mod hooks;
mod tool;

pub use agent_loop::{AgentLoop, AgentLoopConfig, AgentRunResult, ToolExecutionMode};
pub use hooks::{AfterToolCallHook, BeforeToolCallHook, BeforeToolCallResult, ToolResultOverride};
pub use tool::{Tool, ToolError, ToolRegistry};

#[cfg(test)]
mod tests;
