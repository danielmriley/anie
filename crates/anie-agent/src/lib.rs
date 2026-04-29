//! Core agent loop, tool contracts, and execution hooks for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod agent_loop;
mod hooks;
mod tool;

pub use agent_loop::{
    AgentLoop, AgentLoopConfig, AgentRunMachine, AgentRunResult, AgentStepBoundary, CompactionGate,
    CompactionGateOutcome, ToolExecutionMode, send_event,
};
pub use tool::{
    MIN_TOOL_OUTPUT_BUDGET_BYTES, Tool, ToolError, ToolExecutionContext, ToolRegistry,
    effective_tool_output_budget,
};

#[cfg(test)]
mod tests;
