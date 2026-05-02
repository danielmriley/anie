//! Core agent loop, tool contracts, and execution hooks for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod agent_loop;
mod failure_loop;
mod hooks;
mod recurse;
mod recurse_depth;
mod tool;

pub use agent_loop::{
    AgentLoop, AgentLoopConfig, AgentRunMachine, AgentRunResult, AgentStepBoundary,
    BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse, CompactionGate,
    CompactionGateOutcome, NoopBeforeModelPolicy, ToolExecutionMode, send_event,
};
pub use failure_loop::DEFAULT_FAILURE_LOOP_THRESHOLD;
pub use recurse_depth::DEFAULT_RECURSE_DEPTH_WARN_AT;
pub use recurse::{ContextProvider, RecurseScope, SubAgentBuildContext, SubAgentFactory};
pub use tool::{
    MIN_TOOL_OUTPUT_BUDGET_BYTES, Tool, ToolError, ToolExecutionContext, ToolRegistry,
    effective_tool_output_budget,
};

#[cfg(test)]
mod tests;
