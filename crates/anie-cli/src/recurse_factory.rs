//! Controller-side [`SubAgentFactory`] implementation.
//!
//! Builds a fresh `AgentLoop` per recurse sub-call.
//!
//! PR 2 of `docs/rlm_subagents_2026-05-01/`: sub-agents now
//! inherit a **filtered** copy of the parent's tool registry
//! instead of getting an empty one. This unlocks the rest of
//! the sub-agents work — a sub-agent solving a sub-problem
//! can now run `bash`, read files, edit code, hit web tools,
//! etc., independently. Without this, sub-agents were
//! pattern-matchers on the parent's archive; with it, they're
//! actual specialist agents.
//!
//! Filter rules:
//! - **Always inherit:** `bash`, `read`, `edit`, `write`,
//!   `grep`, `find`, `ls`, `web_search`, `web_read`, `skill`.
//!   Self-contained tools that are safe for sub-agents to use.
//! - **Conditionally inherit `recurse`:** only when
//!   `ctx.depth < recurse_depth_limit_for_inheritance`
//!   (default 3, configurable via
//!   `ANIE_RECURSE_DEPTH_LIMIT_FOR_INHERITANCE`). Beyond that
//!   depth a sub-agent has to solve its problem with the other
//!   tools or terminate without recursing. This is a soft cap
//!   on `recurse` only, NOT a hard cap on overall recursion
//!   depth — depth tracking remains observability-only (PR 1).
//!
//! Compaction gate / before-model policy still default to
//! `None` for sub-agents — those concerns are the parent's.

use std::sync::Arc;

use anyhow::Result;

use anie_agent::{
    AgentLoop, AgentLoopConfig, SubAgentBuildContext, SubAgentFactory, ToolExecutionMode,
    ToolRegistry,
};
use anie_provider::{Model, ProviderRegistry, RequestOptionsResolver, ThinkingLevel};

/// Default depth at which sub-agents stop inheriting
/// `recurse`. The controller can override via
/// `ANIE_RECURSE_DEPTH_LIMIT_FOR_INHERITANCE`.
pub(crate) const DEFAULT_RECURSE_INHERIT_LIMIT: u8 = 3;

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
    /// Parent's tool registry — sub-agents inherit a filtered
    /// subset. PR 2 of `docs/rlm_subagents_2026-05-01/`.
    pub parent_tools: Arc<ToolRegistry>,
    /// Soft cap on inheriting `recurse`. At
    /// `ctx.depth >= this`, recurse is filtered out so the
    /// sub-agent can't fork further. Defaults to
    /// `DEFAULT_RECURSE_INHERIT_LIMIT`; set from
    /// `ANIE_RECURSE_DEPTH_LIMIT_FOR_INHERITANCE` at bootstrap.
    pub recurse_inherit_limit: u8,
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
            // Sequential — sub-agents are typically focused on
            // one scope; parallel tool execution adds overhead
            // without a clear win. The parent runs Parallel.
            ToolExecutionMode::Sequential,
            Arc::clone(&self.request_options_resolver),
        )
        .with_ollama_num_ctx_override(self.ollama_num_ctx_override);
        let inherited = build_inherited_registry(
            &self.parent_tools,
            ctx.depth,
            self.recurse_inherit_limit,
        );
        Ok(AgentLoop::new(
            Arc::clone(&self.provider_registry),
            inherited,
            config,
        ))
    }
}

/// Build a sub-agent tool registry by filtering the parent's.
/// Pulled out for direct testability.
fn build_inherited_registry(
    parent: &ToolRegistry,
    depth: u8,
    recurse_inherit_limit: u8,
) -> Arc<ToolRegistry> {
    let mut new_registry = ToolRegistry::new();
    for def in parent.definitions() {
        if !should_inherit(&def.name, depth, recurse_inherit_limit) {
            continue;
        }
        if let Some(tool) = parent.get(&def.name) {
            new_registry.register(tool);
        }
    }
    Arc::new(new_registry)
}

/// Decide whether a tool should be inherited by a sub-agent.
fn should_inherit(tool_name: &str, depth: u8, recurse_inherit_limit: u8) -> bool {
    match tool_name {
        // Always-inherited tools: self-contained, safe for
        // sub-agents to use independently.
        "bash" | "read" | "edit" | "write" | "grep" | "find" | "ls" | "web_search"
        | "web_read" | "skill" => true,
        // recurse: depth-gated.
        "recurse" => depth < recurse_inherit_limit,
        // Unknown / future tools: default to NOT inheriting.
        // Forces a deliberate decision for new tools instead of
        // leaking access by accident.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_agent::{Tool, ToolError, ToolExecutionContext};
    use anie_protocol::{ContentBlock, ToolDef};
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    /// Test tool that does nothing — we only need it to populate
    /// the parent registry with named tools so we can verify the
    /// filter logic.
    struct NamedTool {
        name: String,
    }

    #[async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: self.name.clone(),
                description: "test".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _call_id: &str,
            _args: serde_json::Value,
            _cancel: CancellationToken,
            _update_tx: Option<mpsc::Sender<anie_protocol::ToolResult>>,
            _ctx: &ToolExecutionContext,
        ) -> Result<anie_protocol::ToolResult, ToolError> {
            Ok(anie_protocol::ToolResult {
                content: vec![ContentBlock::Text { text: "ok".into() }],
                details: serde_json::Value::Null,
            })
        }
    }

    fn registry_with(names: &[&str]) -> Arc<ToolRegistry> {
        let mut r = ToolRegistry::new();
        for name in names {
            r.register(Arc::new(NamedTool { name: (*name).into() }));
        }
        Arc::new(r)
    }

    #[test]
    fn always_inherit_self_contained_tools() {
        let parent = registry_with(&[
            "bash", "read", "edit", "write", "grep", "find", "ls", "web_search", "web_read",
            "skill",
        ]);
        let inherited = build_inherited_registry(&parent, /*depth*/ 0, /*limit*/ 3);
        let names: Vec<String> = inherited.definitions().into_iter().map(|d| d.name).collect();
        for expected in [
            "bash", "read", "edit", "write", "grep", "find", "ls", "web_search", "web_read",
            "skill",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "{expected} should always inherit; got {names:?}"
            );
        }
    }

    #[test]
    fn recurse_inherits_when_depth_below_limit() {
        let parent = registry_with(&["recurse", "bash"]);
        let inherited = build_inherited_registry(&parent, /*depth*/ 2, /*limit*/ 3);
        let names: Vec<String> = inherited.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"recurse".to_string()), "{names:?}");
    }

    #[test]
    fn recurse_filtered_when_depth_at_limit() {
        let parent = registry_with(&["recurse", "bash"]);
        let inherited = build_inherited_registry(&parent, /*depth*/ 3, /*limit*/ 3);
        let names: Vec<String> = inherited.definitions().into_iter().map(|d| d.name).collect();
        assert!(!names.contains(&"recurse".to_string()), "{names:?}");
        // bash still inherits at any depth.
        assert!(names.contains(&"bash".to_string()), "{names:?}");
    }

    #[test]
    fn recurse_filtered_when_depth_above_limit() {
        let parent = registry_with(&["recurse"]);
        let inherited = build_inherited_registry(&parent, /*depth*/ 5, /*limit*/ 3);
        let names: Vec<String> = inherited.definitions().into_iter().map(|d| d.name).collect();
        assert!(!names.contains(&"recurse".to_string()), "{names:?}");
    }

    #[test]
    fn unknown_tools_do_not_inherit_by_default() {
        // A future tool we haven't yet decided about should
        // NOT leak into sub-agents — forces us to update the
        // filter intentionally.
        let parent = registry_with(&["bash", "future_dangerous_tool"]);
        let inherited = build_inherited_registry(&parent, /*depth*/ 0, /*limit*/ 3);
        let names: Vec<String> = inherited.definitions().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"bash".to_string()));
        assert!(!names.contains(&"future_dangerous_tool".to_string()), "{names:?}");
    }
}
