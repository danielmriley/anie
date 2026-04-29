//! Controller-side [`SubAgentFactory`] implementation.
//!
//! Builds a fresh `AgentLoop` per recurse sub-call. In this
//! commit (`rlm/05`) sub-agents are deliberately minimal:
//!
//! - **No tools** in the sub-agent's tool registry. The
//!   sub-agent's only job is to answer a focused sub-query
//!   from the messages it was given. It can produce text but
//!   not call further tools (including no recurse-in-recurse).
//!   That keeps the depth=1 case clean and tractable.
//! - **No compaction gate.** Sub-calls are expected to fit
//!   in a single turn; if a sub-call's context is large
//!   enough to need compaction, the parent should have
//!   chosen a tighter scope.
//! - **No `BeforeModelPolicy` hooks.** The default noop is
//!   used; sub-agents don't participate in the active-context
//!   ceiling that Phase C of Plan 06 will install for the
//!   parent.
//!
//! Future commits will gate `recurse` registration on
//! `ctx.depth < max_depth` so depth=2 sub-agents can recurse
//! once more, plus add a `SystemPromptKind::SubAgent` variant
//! tuned for recursion.

use std::sync::Arc;

use anyhow::Result;

use anie_agent::{
    AgentLoop, AgentLoopConfig, SubAgentBuildContext, SubAgentFactory, ToolExecutionMode,
    ToolRegistry,
};
use anie_provider::{Model, ProviderRegistry, RequestOptionsResolver, ThinkingLevel};

/// Snapshot of the parent run's agent configuration; consulted
/// by `build` to construct a sub-agent that mirrors the parent
/// modulo the deliberate minimizations listed in the module
/// doc.
pub(crate) struct ControllerSubAgentFactory {
    pub provider_registry: Arc<ProviderRegistry>,
    pub model: Model,
    pub system_prompt: String,
    pub thinking: ThinkingLevel,
    pub request_options_resolver: Arc<dyn RequestOptionsResolver>,
    pub ollama_num_ctx_override: Option<u64>,
}

impl SubAgentFactory for ControllerSubAgentFactory {
    fn build(&self, ctx: &SubAgentBuildContext) -> Result<AgentLoop> {
        let model = ctx
            .model_override
            .clone()
            .unwrap_or_else(|| self.model.clone());
        let config = AgentLoopConfig::new(
            model,
            self.system_prompt.clone(),
            self.thinking,
            // Sequential: a sub-agent processing a focused
            // scope doesn't need parallel tool execution. It
            // doesn't have any tools anyway.
            ToolExecutionMode::Sequential,
            Arc::clone(&self.request_options_resolver),
        )
        .with_ollama_num_ctx_override(self.ollama_num_ctx_override);
        Ok(AgentLoop::new(
            Arc::clone(&self.provider_registry),
            // Empty tool registry — sub-agents in this commit
            // are non-recursive and tool-free. Plan 06 Phase
            // A's `max_depth` enforcement (gate on
            // `ctx.depth < max_depth`) lands when we add a
            // depth-aware tool selection.
            Arc::new(ToolRegistry::new()),
            config,
        ))
    }
}
