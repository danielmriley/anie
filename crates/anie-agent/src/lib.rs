//! Core agent loop, tool contracts, and execution hooks for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod agent_loop;
mod hooks;
mod tool;

pub use agent_loop::{
    AgentLoop, AgentLoopConfig, AgentRunResult, CompactionGate, CompactionGateOutcome,
    ToolExecutionMode, send_event,
};
pub use tool::{Tool, ToolError, ToolRegistry};

#[cfg(test)]
mod tests;
