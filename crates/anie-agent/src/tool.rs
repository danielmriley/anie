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
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Create an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool, replacing any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.definition().name.clone(), tool);
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Return all tool definitions in deterministic name order.
    #[must_use]
    pub fn definitions(&self) -> Vec<ToolDef> {
        let mut definitions: Vec<_> = self.tools.values().map(|tool| tool.definition()).collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
