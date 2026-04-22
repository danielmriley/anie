use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_protocol::{ToolDef, ToolResult};

/// Trait implemented by every tool.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Return the tool definition exposed to the model.
    fn definition(&self) -> ToolDef;

    /// Execute the tool with validated JSON arguments.
    async fn execute(
        &self,
        call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        update_tx: Option<mpsc::Sender<ToolResult>>,
    ) -> Result<ToolResult, ToolError>;
}

/// Structured tool failures.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ToolError {
    /// The tool failed while executing.
    #[error("{0}")]
    ExecutionFailed(String),
    /// The tool was aborted.
    #[error("Tool execution aborted")]
    Aborted,
    /// The tool timed out.
    #[error("Timeout after {0} seconds")]
    Timeout(u64),
}

/// Registry of tools keyed by name.
///
/// Tool metadata is effectively immutable after startup-time
/// registration, so the registry caches the sorted definition
/// list and rebuilds it only when `register()` runs. The
/// hot-path `definitions()` call is then a single vector
/// clone rather than a per-call collect + sort.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    /// Cached snapshot of every registered tool's `ToolDef`,
    /// sorted by name. Rebuilt on every `register()` —
    /// registration happens only at startup, so the rebuild
    /// is paid exactly once per tool.
    sorted_definitions: Vec<ToolDef>,
}

impl ToolRegistry {
    /// Create an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            sorted_definitions: Vec::new(),
        }
    }

    /// Register a tool, replacing any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name;
        self.tools.insert(name, tool);
        self.rebuild_sorted_definitions();
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Return all tool definitions in deterministic name order.
    ///
    /// Returns a clone of the cached sorted list; no per-call
    /// sort. Callers that only need a borrow can use
    /// [`Self::definitions_borrowed`].
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDef> {
        self.sorted_definitions.clone()
    }

    /// Borrow the cached sorted definition list. Zero-allocation
    /// alternative to [`Self::definitions`] for callers that
    /// only need read access.
    #[must_use]
    pub fn definitions_borrowed(&self) -> &[ToolDef] {
        &self.sorted_definitions
    }

    fn rebuild_sorted_definitions(&mut self) {
        let mut definitions: Vec<_> = self.tools.values().map(|tool| tool.definition()).collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        self.sorted_definitions = definitions;
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct StubTool {
        def: ToolDef,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn definition(&self) -> ToolDef {
            self.def.clone()
        }

        async fn execute(
            &self,
            _call_id: &str,
            _args: serde_json::Value,
            _cancel: CancellationToken,
            _update_tx: Option<mpsc::Sender<ToolResult>>,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("tests do not exercise execution")
        }
    }

    fn stub(name: &str) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            def: ToolDef {
                name: name.to_string(),
                description: format!("desc for {name}"),
                parameters: serde_json::json!({"type": "object"}),
            },
        })
    }

    #[test]
    fn tool_registry_definitions_are_sorted_once_and_stable() {
        // Register out of alphabetical order. Repeated
        // `definitions()` calls must return the same sequence
        // each time — proves we're not re-sorting against a
        // HashMap's arbitrary iteration order.
        let mut registry = ToolRegistry::new();
        registry.register(stub("zed"));
        registry.register(stub("alpha"));
        registry.register(stub("middle"));

        let first = registry.definitions();
        let second = registry.definitions();
        let third = registry.definitions();

        let names: Vec<_> = first.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "middle", "zed"]);
        assert_eq!(first, second);
        assert_eq!(second, third);
    }

    #[test]
    fn tool_registry_returns_cached_definitions_in_registration_order_after_sort() {
        // Same invariant from the other end: insertion order
        // must not bleed through when it collides with
        // alphabetical order.
        let mut registry = ToolRegistry::new();
        registry.register(stub("alpha"));
        registry.register(stub("beta"));
        registry.register(stub("gamma"));

        let names: Vec<_> = registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn tool_registry_definitions_borrowed_matches_owned_snapshot() {
        let mut registry = ToolRegistry::new();
        registry.register(stub("b"));
        registry.register(stub("a"));
        assert_eq!(registry.definitions(), registry.definitions_borrowed());
    }

    #[test]
    fn tool_registry_register_replaces_existing_and_rebuilds_cache() {
        // Registering the same name twice must not duplicate
        // entries in the cache — replacement semantics are
        // preserved.
        let mut registry = ToolRegistry::new();
        registry.register(stub("tool"));
        registry.register(stub("tool"));
        assert_eq!(registry.definitions().len(), 1);
    }
}
