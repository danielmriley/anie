use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use jsonschema::Validator;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_protocol::{ToolDef, ToolResult};

/// Result of compiling a tool's JSON schema at registration
/// time. `Invalid` preserves current semantics: the tool
/// stays registered so the error is surfaced on first use,
/// but we don't hide it behind a lazy recompile per call.
pub enum ValidatorState {
    /// The schema compiled successfully.
    Ready(Arc<Validator>),
    /// The schema failed to compile. The stored message is
    /// surfaced verbatim on any tool call — identical wording
    /// to what the old inline compilation would have returned.
    Invalid(String),
}

/// Per-execution context handed to every `Tool::execute` call.
/// Carries metadata the agent loop knows about the current
/// invocation; tools ignore fields they don't care about.
///
/// Plan `docs/midturn_compaction_2026-04-27/05_tool_output_caps_scale_with_context.md`
/// PR A: introduce the struct and thread it through. Output-
/// scaling logic that consumes `context_window` lands in PR B+.
#[derive(Debug, Clone, Copy)]
pub struct ToolExecutionContext {
    /// Effective context window for the current model
    /// (post-`/context-length`-override). Tools use this to
    /// scale output budgets via
    /// `effective_tool_output_budget`.
    pub context_window: u64,
}

impl ToolExecutionContext {
    /// Default-constructor for tests and callers that don't
    /// participate in the agent loop's plumbing. The 200K
    /// value is intentionally large so it's effectively a
    /// no-op for any output budget that would otherwise
    /// shrink on smaller windows — existing tool behavior is
    /// preserved.
    pub const TEST_DEFAULT: Self = Self {
        context_window: 200_000,
    };
}

impl Default for ToolExecutionContext {
    fn default() -> Self {
        Self::TEST_DEFAULT
    }
}

/// Floor on the per-tool output budget. A pathologically
/// small context window must not push the budget below a
/// handful of useful lines; otherwise tools return only an
/// elision marker and the agent has nothing to act on.
/// Same shape as `compaction_reserve::DEFAULT_MIN_RESERVE_TOKENS`
/// over in `anie-cli`: a hard floor that keeps very small
/// windows usable.
pub const MIN_TOOL_OUTPUT_BUDGET_BYTES: u64 = 1_024;

/// Numerator/denominator of the share of the context window
/// any single tool result is allowed to occupy. With a 10 %
/// share, a single tool result claiming more than 30 % of the
/// model's context is mechanically impossible — and 30 % is
/// already the upper end of what's tolerable. This factor is
/// not yet user-configurable; if real workloads ask for a
/// different ratio, expose it via `[tools]` config (see plan
/// 05's deferred items).
const CONTEXT_SHARE_NUMERATOR: u64 = 1;
const CONTEXT_SHARE_DENOMINATOR: u64 = 10;

/// Compute the effective output budget for a single tool
/// result given the model's context window and the tool's
/// hard-coded `base_default` cap.
///
/// Three rules apply, in order:
///
/// 1. Take ~10 % of `context_window` as the context-share
///    budget.
/// 2. Cap that share by `base_default` so we never *grow* the
///    budget for cloud-window models — the existing constant
///    remains the upper bound.
/// 3. Floor at [`MIN_TOOL_OUTPUT_BUDGET_BYTES`] so that
///    pathologically small windows don't return an empty
///    result. (Even a 4K-window model can usefully ingest
///    1 KB of stdout.)
///
/// Examples (`base_default = 50 KiB`):
///
/// | window  | share  | clamped to base | floored | result   |
/// |---------|--------|-----------------|---------|----------|
/// | 200,000 | 20,000 | 20,000          | 20,000  | 20,000   |
/// | 65,536  |  6,553 |  6,553          |  6,553  |  6,553   |
/// | 16,384  |  1,638 |  1,638          |  1,638  |  1,638   |
/// |  8,192  |    819 |    819          |  1,024  |  1,024   |
/// |  4,096  |    409 |    409          |  1,024  |  1,024   |
///
/// Plan 05 of `docs/midturn_compaction_2026-04-27/`.
#[must_use]
pub fn effective_tool_output_budget(context_window: u64, base_default: u64) -> u64 {
    let share = context_window.saturating_mul(CONTEXT_SHARE_NUMERATOR) / CONTEXT_SHARE_DENOMINATOR;
    let capped = share.min(base_default);
    capped.max(MIN_TOOL_OUTPUT_BUDGET_BYTES)
}

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
        ctx: &ToolExecutionContext,
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
    /// Precompiled JSON schema validators keyed by tool name.
    /// Compiled once in `register()`; reused on every tool call.
    /// If compilation failed, the stored error message is
    /// returned verbatim on use.
    validators: HashMap<String, ValidatorState>,
}

impl ToolRegistry {
    /// Create an empty tool registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            sorted_definitions: Vec::new(),
            validators: HashMap::new(),
        }
    }

    /// Register a tool, replacing any existing tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let definition = tool.definition();
        let name = definition.name.clone();
        let validator_state = match jsonschema::validator_for(&definition.parameters) {
            Ok(validator) => ValidatorState::Ready(Arc::new(validator)),
            Err(error) => {
                ValidatorState::Invalid(format!("Tool schema compilation failed: {error}"))
            }
        };
        self.validators.insert(name.clone(), validator_state);
        self.tools.insert(name, tool);
        self.rebuild_sorted_definitions();
    }

    /// Return the precompiled validator state for a registered
    /// tool, or `None` if no tool with that name is registered.
    #[must_use]
    pub fn validator(&self, name: &str) -> Option<&ValidatorState> {
        self.validators.get(name)
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
            _ctx: &ToolExecutionContext,
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

    /// A tool with a valid schema gets a `Ready` validator
    /// precompiled at registration time; no runtime compile.
    #[test]
    fn tool_registry_compiles_valid_schema_at_registration() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool {
            def: ToolDef {
                name: "echo".into(),
                description: "e".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"msg": {"type": "string"}},
                    "required": ["msg"]
                }),
            },
        }));
        match registry.validator("echo") {
            Some(ValidatorState::Ready(_)) => {}
            Some(ValidatorState::Invalid(msg)) => panic!("expected Ready, got Invalid({msg})"),
            None => panic!("expected Ready, got missing validator"),
        }
    }

    /// Plan 05 PR B: the effective output budget must:
    ///
    /// 1. Stay below the configured `base_default` for cloud
    ///    windows (a 200K window's 10 % share is 20K, well
    ///    under the 50K bash cap).
    /// 2. Shrink linearly for medium windows.
    /// 3. Floor at `MIN_TOOL_OUTPUT_BUDGET_BYTES` for very
    ///    small windows so tools return *something* useful
    ///    rather than an empty result.
    #[test]
    fn effective_tool_output_budget_clamps_to_context_share_for_large_windows() {
        // 200K context, 50K base: 10% share = 20K, picks share.
        assert_eq!(effective_tool_output_budget(200_000, 50_000), 20_000);
        // 65K context, 50K base: share = 6553, picks share.
        assert_eq!(effective_tool_output_budget(65_536, 50_000), 6_553);
    }

    #[test]
    fn effective_tool_output_budget_floors_at_min_budget_for_small_windows() {
        // 8K window: share = 819, floored to 1024.
        assert_eq!(
            effective_tool_output_budget(8_192, 50_000),
            MIN_TOOL_OUTPUT_BUDGET_BYTES
        );
        // 4K window: share = 409, floored to 1024.
        assert_eq!(
            effective_tool_output_budget(4_096, 50_000),
            MIN_TOOL_OUTPUT_BUDGET_BYTES
        );
    }

    #[test]
    fn effective_tool_output_budget_never_exceeds_base_default() {
        // Even a huge window cannot grow the budget above the
        // tool's own configured cap.
        assert_eq!(effective_tool_output_budget(2_000_000, 50_000), 50_000);
    }

    #[test]
    fn effective_tool_output_budget_handles_zero_window_with_floor() {
        // Degenerate config (window 0) should not crash and
        // should produce the floor.
        assert_eq!(
            effective_tool_output_budget(0, 50_000),
            MIN_TOOL_OUTPUT_BUDGET_BYTES
        );
    }

    /// An invalid schema is stored as `Invalid(msg)` so the
    /// error surfaces on first use rather than being retried
    /// (and failing again) on every call.
    #[test]
    fn tool_registry_stores_invalid_schema_as_stringified_error() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool {
            def: ToolDef {
                name: "broken".into(),
                description: "b".into(),
                // `type` must be a string or array, not an int.
                parameters: serde_json::json!({"type": 42}),
            },
        }));
        match registry.validator("broken") {
            Some(ValidatorState::Invalid(msg)) => {
                assert!(
                    msg.starts_with("Tool schema compilation failed:"),
                    "legacy error prefix must be preserved: {msg}"
                );
            }
            _ => panic!("expected Invalid for malformed schema"),
        }
    }
}
