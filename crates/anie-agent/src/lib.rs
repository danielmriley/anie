//! Core agent loop, tool contracts, and execution hooks for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod agent_loop;
mod arg_coerce;
mod evidence;
mod failure_loop;
mod recurse;
mod recurse_depth;
mod stream_builder;
mod tool;
mod tool_call_parse;

pub use agent_loop::{
    AgentLoop, AgentLoopConfig, AgentRunMachine, AgentRunResult, AgentStepBoundary,
    BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse, BudgetLimit, BudgetScope,
    ChainedBeforeModelPolicy, CompactionGate, CompactionGateOutcome, MAX_TOOL_REPAIR_ROUNDS,
    NoopBeforeModelPolicy, RunStopReason, ToolExecutionMode, send_event,
};
pub use evidence::{
    EVIDENCE_FINAL_ANSWER_STANCE, ObservedEvidence, ObservedFact, ObservedKind,
    collect_observed_evidence, render_evidence_brief,
};
pub use failure_loop::{DEFAULT_FAILURE_LOOP_THRESHOLD, stable_args_hash};
pub use recurse::{ContextProvider, RecurseScope, SubAgentBuildContext, SubAgentFactory};
pub use recurse_depth::DEFAULT_RECURSE_DEPTH_WARN_AT;
pub use tool::{
    MIN_TOOL_OUTPUT_BUDGET_BYTES, Tool, ToolError, ToolExecutionContext, ToolRegistry,
    ValidatorState, effective_tool_output_budget,
};
pub use tool_call_parse::{
    EmbeddedToolCallFormat, ResolvedToolCalls, ToolCallParse, parse_embedded_tool_calls,
    parse_repair_prompt, resolve_assistant_tool_calls,
};

#[cfg(test)]
mod tests;
