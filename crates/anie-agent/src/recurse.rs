//! Trait surface for the RLM `recurse` tool.
//!
//! The `recurse` tool lets the model call itself with a
//! focused subset of context, so it can navigate a large
//! external corpus (the run's accumulated messages, files on
//! disk, prior tool results) without having to fit the whole
//! thing in the active context window. See the paper at
//! [arXiv 2512.24601](https://arxiv.org/abs/2512.24601) for
//! the foundational idea, and `docs/rlm_2026-04-29/` for
//! anie's adoption plan.
//!
//! This module holds the *abstraction layer*: the data type
//! describing a recurse scope, plus the two traits the tool
//! depends on.
//!
//! - [`RecurseScope`] — what slice of context the model is
//!   asking to see.
//! - [`ContextProvider`] — resolves a scope to concrete
//!   messages. Implemented by the controller, which holds
//!   the parent run's context view + filesystem access.
//! - [`SubAgentFactory`] — constructs the child `AgentLoop`
//!   the recurse tool drives. Implemented by the controller,
//!   which knows how to clone its agent config with the right
//!   per-sub-call adjustments (system prompt, model
//!   override, tool registry gated at max depth).
//!
//! Plan: `docs/rlm_2026-04-29/02_recurse_tool.md` and
//! `docs/rlm_2026-04-29/06_phased_implementation.md` Phase A.

use std::sync::{Arc, atomic::AtomicU32};

use anyhow::Result;
use async_trait::async_trait;

use anie_protocol::Message;
use anie_provider::Model;

use crate::AgentLoop;

/// Identifies a region of context the sub-agent should see
/// as its initial messages.
///
/// The recurse tool's JSON argument schema (added in a later
/// commit when the tool itself lands) maps to these variants.
/// For now the type is the contract `ContextProvider` resolves
/// against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurseScope {
    /// Messages by index range from the parent run's context.
    /// Half-open: `[start, end)`. Out-of-bounds indices are a
    /// resolver error; empty ranges are not.
    MessageRange {
        /// Inclusive lower bound.
        start: usize,
        /// Exclusive upper bound.
        end: usize,
    },
    /// Messages whose content matches a regex pattern.
    /// Compilation happens at resolve time; an invalid
    /// pattern surfaces as a resolver error rather than a
    /// scope-construction error.
    MessageGrep {
        /// Anchored or unanchored regex; the resolver
        /// compiles with the `regex` crate's default flags.
        pattern: String,
    },
    /// One specific tool result message, addressed by its
    /// `tool_call_id` (the same id the assistant message used
    /// when emitting the tool call).
    ToolResult {
        /// Tool call id from the parent run's history.
        tool_call_id: String,
    },
    /// File contents on disk, wrapped as a single
    /// `Message::User` so the sub-agent's prompt format is
    /// uniform across scope kinds. The resolver applies a
    /// configurable byte cap (default: same as web_read) so
    /// pointing recurse at a 1GB log file doesn't OOM.
    File {
        /// Path on disk; relative paths resolve against the
        /// run's working directory.
        path: String,
    },
}

impl RecurseScope {
    /// Stable string discriminant for tracing fields and JSON
    /// arg serialization. Public so consumers (the recurse
    /// tool, tracing instrumentation) can use one source of
    /// truth.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::MessageRange { .. } => "message_range",
            Self::MessageGrep { .. } => "message_grep",
            Self::ToolResult { .. } => "tool_result",
            Self::File { .. } => "file",
        }
    }
}

/// Per-sub-call build inputs threaded from the recurse tool
/// into the controller's [`SubAgentFactory`] implementation.
///
/// The factory uses these to decide:
/// - Whether the sub-agent's tool registry should include
///   `recurse` again (gated at `depth >= max_depth`).
/// - Which model to use (defaults to the parent's; the
///   override is for future use, e.g., a cheap summarizer
///   model for shallow sub-calls).
/// - Where to point the recursion budget atomic so this
///   sub-call's own recurse calls share the same counter.
pub struct SubAgentBuildContext {
    /// Current recursion depth. `0` is the top-level run;
    /// `1` is the first sub-call; etc. Implementations
    /// decide whether `depth >= max_depth` should drop the
    /// recurse tool from the sub-agent's registry.
    pub depth: u8,
    /// Recursion budget shared across all sub-calls in a
    /// single top-level run. Each `recurse` invocation
    /// decrements; when zero, the tool errors instead of
    /// building a new sub-agent. The factory does not
    /// decrement — that's the tool's job — but it threads
    /// the same `Arc` into the sub-agent so deeper recursion
    /// shares the counter.
    pub recursion_budget: Arc<AtomicU32>,
    /// Optional model override for this sub-call. `None`
    /// means "use the parent's model." Future use: a
    /// smaller summarizer-class model for cheap sub-calls,
    /// or a recursion-trained model for natively-RLM mode
    /// (Plan 04).
    pub model_override: Option<Model>,
}

/// Builds an [`AgentLoop`] for a recurse-tool sub-call.
///
/// The controller is the canonical implementer because it's
/// the only place that knows the parent run's full
/// configuration (provider registry, model, tool registry,
/// system prompt). Test code can implement this trait with a
/// stub factory that returns a pre-built loop.
pub trait SubAgentFactory: Send + Sync {
    /// Construct a sub-agent loop given the build context.
    /// Errors propagate to the recurse tool, which surfaces
    /// them to the model as a tool error.
    fn build(&self, ctx: &SubAgentBuildContext) -> Result<AgentLoop>;
}

/// Resolves a [`RecurseScope`] to the concrete messages the
/// sub-agent should see as its initial context.
///
/// The controller is again the canonical implementer: it
/// holds an `Arc<RwLock<Vec<Message>>>` view of the parent's
/// active context (synced after each REPL `Print` step), plus
/// filesystem access for `RecurseScope::File`.
#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// Map a scope to messages. Errors include:
    /// - `MessageRange` indices out of bounds.
    /// - `MessageGrep` invalid regex pattern.
    /// - `ToolResult` `tool_call_id` not found.
    /// - `File` path missing, unreadable, or beyond the
    ///   configured byte cap.
    async fn resolve(&self, scope: &RecurseScope) -> Result<Vec<Message>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stable discriminant strings — used in tracing fields
    /// and JSON tool-arg serialization. Tightening this
    /// contract here means the discriminants are not
    /// accidentally renamed by a future refactor that doesn't
    /// realize they're load-bearing.
    #[test]
    fn recurse_scope_kind_is_stable() {
        assert_eq!(
            RecurseScope::MessageRange { start: 0, end: 5 }.kind(),
            "message_range"
        );
        assert_eq!(
            RecurseScope::MessageGrep {
                pattern: "weather".into()
            }
            .kind(),
            "message_grep"
        );
        assert_eq!(
            RecurseScope::ToolResult {
                tool_call_id: "call_abc".into()
            }
            .kind(),
            "tool_result"
        );
        assert_eq!(
            RecurseScope::File {
                path: "/tmp/x".into()
            }
            .kind(),
            "file"
        );
    }
}
