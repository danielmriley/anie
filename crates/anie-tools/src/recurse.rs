//! `recurse` tool — RLM Phase A.
//!
//! Lets the model invoke a focused sub-agent over a slice of
//! its own prior context (or, in later commits, files /
//! tool-result entries by id). The sub-agent runs as a
//! self-contained `AgentRunMachine` with the resolved
//! messages plus the model's sub-query as its only context;
//! its final assistant text becomes the tool result the
//! parent agent sees.
//!
//! From the model's perspective recurse is a normal tool
//! call: it sends a sub-query plus a scope, receives a
//! single-text-block result. The recursion is invisible to
//! the master event stream — sub-agent events are drained
//! into a no-op consumer (Plan 06 calls this "opaque sub-call,
//! revisit later if the UX feels mysterious").
//!
//! Plan: `docs/rlm_2026-04-29/02_recurse_tool.md` and
//! `docs/rlm_2026-04-29/06_phased_implementation.md` Phase A.

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use anie_agent::{
    ContextProvider, RecurseScope, SubAgentBuildContext, SubAgentFactory, Tool, ToolError,
    ToolExecutionContext,
};
use anie_protocol::{
    AgentEvent, ContentBlock, Message, ToolDef, ToolResult, UserMessage, now_millis,
};

use crate::shared::text_result;

/// `recurse` tool implementation.
///
/// Construction is dependency-injected: the factory and
/// provider come from the controller. The recursion budget is
/// shared (`Arc<AtomicU32>`) across every recurse instance in
/// a single top-level run; sub-agents at depth N see the
/// same atomic so deeper recursion shares the counter.
pub struct RecurseTool {
    sub_agent_factory: Arc<dyn SubAgentFactory>,
    context_provider: Arc<dyn ContextProvider>,
    /// Shared recursion budget. Each invocation decrements;
    /// when zero, the tool returns an error result rather
    /// than building a sub-agent.
    recursion_budget: Arc<AtomicU32>,
    /// Current depth (0 = top-level run, 1 = first sub-call,
    /// etc.). Threaded into `SubAgentBuildContext` so the
    /// factory can decide whether to register `recurse`
    /// again.
    depth: u8,
    /// At `depth >= max_depth`, the factory should drop
    /// `recurse` from the sub-agent's tool registry. The
    /// tool itself doesn't enforce this — that's the
    /// factory's job — but it carries `max_depth` along so
    /// the factory has the information.
    max_depth: u8,
}

impl RecurseTool {
    /// Build a recurse tool from the wiring the controller
    /// provides per run.
    #[must_use]
    pub fn new(
        sub_agent_factory: Arc<dyn SubAgentFactory>,
        context_provider: Arc<dyn ContextProvider>,
        recursion_budget: Arc<AtomicU32>,
        depth: u8,
        max_depth: u8,
    ) -> Self {
        Self {
            sub_agent_factory,
            context_provider,
            recursion_budget,
            depth,
            max_depth,
        }
    }
}

#[async_trait]
impl Tool for RecurseTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "recurse".into(),
            description: "Invoke a focused sub-agent over a scoped slice of context to answer a sub-query, returning the sub-agent's final answer as text. Use this when you need to navigate prior conversation, prior tool results, or external files without loading them all into your active context. The sub-agent runs in isolation: it sees only the messages selected by `scope` plus your `query`, and its output is summarized into a single tool-result block. Available scope kinds: `message_range` ({start, end} half-open indices into your prior context), `message_grep` ({pattern} regex over message content), `tool_result` ({tool_call_id} one specific prior tool result), `file` ({path} file contents on disk). Recursion has a per-run budget and a depth limit; when exhausted, the tool errors and you should answer from the active context.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The sub-question for the recursive agent to answer. Should be self-contained; the sub-agent only sees this plus the messages selected by `scope`.",
                    },
                    "scope": {
                        "type": "object",
                        "description": "Which slice of context the sub-agent should see.",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["message_range", "message_grep", "tool_result", "file"],
                                "description": "Discriminator selecting one of the four scope variants.",
                            },
                            "start": {"type": "integer", "description": "message_range: inclusive lower bound (>= 0)."},
                            "end": {"type": "integer", "description": "message_range: exclusive upper bound (>= start)."},
                            "pattern": {"type": "string", "description": "message_grep: regex pattern."},
                            "tool_call_id": {"type": "string", "description": "tool_result: id of the prior tool call."},
                            "path": {"type": "string", "description": "file: path on disk; relative paths resolve against the run cwd."},
                        },
                        "required": ["kind"],
                    },
                },
                "required": ["query", "scope"],
                "additionalProperties": false,
            }),
        }
    }

    async fn execute(
        &self,
        call_id: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        // 1. Parse args. Manual extraction (no serde derive
        //    on the arg types) to avoid pulling serde into
        //    anie-tools. The schema in `definition()` is the
        //    contract; we mirror it here.
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::ExecutionFailed("missing or non-string `query`".into()))?
            .to_string();
        let scope = parse_scope(
            args.get("scope")
                .ok_or_else(|| ToolError::ExecutionFailed("missing `scope` object".into()))?,
        )?;

        info!(
            call_id,
            depth = self.depth,
            scope_kind = scope.kind(),
            query_chars = query.chars().count(),
            "recurse start"
        );

        // 2. Check + decrement recursion budget atomically.
        //    `fetch_update` returns Err if the closure
        //    returns None — we use that to signal exhaustion.
        let budget_take_result =
            self.recursion_budget
                .fetch_update(Ordering::Release, Ordering::Acquire, |n| {
                    if n == 0 { None } else { Some(n - 1) }
                });
        if budget_take_result.is_err() {
            let msg = format!(
                "recurse budget exhausted (depth {}, max depth {}). The model has invoked recurse too many times in this run; either answer from the active context or ask the user.",
                self.depth, self.max_depth,
            );
            info!(call_id, "recurse budget exhausted; returning error result");
            return Err(ToolError::ExecutionFailed(msg));
        }
        let budget_remaining = self.recursion_budget.load(Ordering::Acquire);

        // 3. Resolve the scope to messages.
        let messages = self
            .context_provider
            .resolve(&scope)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("scope resolution failed: {e}")))?;
        debug!(
            call_id,
            resolved_message_count = messages.len(),
            "recurse scope resolved"
        );

        // 4. Build the sub-agent.
        let build_ctx = SubAgentBuildContext {
            depth: self.depth.saturating_add(1),
            recursion_budget: Arc::clone(&self.recursion_budget),
            model_override: None,
        };
        let sub_agent = self
            .sub_agent_factory
            .build(&build_ctx)
            .map_err(|e| ToolError::ExecutionFailed(format!("sub-agent build failed: {e}")))?;

        // 5. Drive the sub-agent to completion. Sub-events
        //    drain into a no-op task — opaque-sub-call
        //    semantics (Plan 06). The parent's cancel token
        //    is passed in directly so a parent abort
        //    propagates.
        let user_prompt = Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: query.clone(),
            }],
            timestamp: now_millis(),
        });
        let (sub_event_tx, mut sub_event_rx) = mpsc::channel::<AgentEvent>(64);
        let drain_task = tokio::spawn(async move {
            while sub_event_rx.recv().await.is_some() {
                // intentionally drop; opaque sub-call
            }
        });
        let mut machine = sub_agent
            .start_run_machine(vec![user_prompt], messages, &sub_event_tx)
            .await;
        while !machine.is_finished() {
            machine.next_step(&sub_event_tx, &cancel).await;
        }
        let sub_result = machine.finish(&sub_event_tx).await;
        drop(sub_event_tx);
        let _ = drain_task.await;

        if let Some(error) = sub_result.terminal_error {
            return Err(ToolError::ExecutionFailed(format!(
                "sub-agent terminated with provider error: {error}"
            )));
        }

        // 6. Extract final assistant text.
        let final_text = extract_final_assistant_text(&sub_result.generated_messages)
            .unwrap_or_else(|| "(sub-agent produced no visible answer)".to_string());

        let details = serde_json::json!({
            "tool": "recurse",
            "scope_kind": scope.kind(),
            "depth": self.depth,
            "sub_call_message_count": sub_result.generated_messages.len(),
            "budget_remaining_after": budget_remaining,
            "max_depth": self.max_depth,
        });
        info!(
            call_id,
            sub_call_message_count = sub_result.generated_messages.len(),
            budget_remaining,
            "recurse done"
        );
        Ok(text_result(final_text, details))
    }
}

/// Parse the `scope` JSON object into a `RecurseScope`.
/// Manual extraction (no serde derive) — matches the rest of
/// anie-tools' style and avoids pulling `serde` derives into
/// the crate's deps.
fn parse_scope(value: &serde_json::Value) -> Result<RecurseScope, ToolError> {
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::ExecutionFailed("scope missing `kind` field".into()))?;
    match kind {
        "message_range" => {
            let start = value
                .get("start")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed("message_range missing `start`".into())
                })?;
            let end = value
                .get("end")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ToolError::ExecutionFailed("message_range missing `end`".into()))?;
            Ok(RecurseScope::MessageRange {
                start: usize::try_from(start).map_err(|_| {
                    ToolError::ExecutionFailed("message_range `start` out of range".into())
                })?,
                end: usize::try_from(end).map_err(|_| {
                    ToolError::ExecutionFailed("message_range `end` out of range".into())
                })?,
            })
        }
        "message_grep" => {
            let pattern = value
                .get("pattern")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed("message_grep missing `pattern`".into())
                })?;
            Ok(RecurseScope::MessageGrep {
                pattern: pattern.to_string(),
            })
        }
        "tool_result" => {
            let tool_call_id = value
                .get("tool_call_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    ToolError::ExecutionFailed("tool_result missing `tool_call_id`".into())
                })?;
            Ok(RecurseScope::ToolResult {
                tool_call_id: tool_call_id.to_string(),
            })
        }
        "file" => {
            let path = value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| ToolError::ExecutionFailed("file missing `path`".into()))?;
            Ok(RecurseScope::File {
                path: path.to_string(),
            })
        }
        other => Err(ToolError::ExecutionFailed(format!(
            "unknown scope kind: {other}",
        ))),
    }
}

/// Pull the last assistant message's first text block out of
/// the sub-agent's generated messages. Returns None when the
/// sub-agent produced only tool calls / tool results with no
/// trailing text — the caller falls back to a placeholder.
fn extract_final_assistant_text(generated: &[Message]) -> Option<String> {
    generated.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => a.content.iter().find_map(|c| match c {
            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.clone()),
            _ => None,
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use anie_agent::{
        AgentLoop, AgentLoopConfig, ContextProvider, RecurseScope, SubAgentBuildContext,
        SubAgentFactory, ToolExecutionMode, ToolRegistry,
    };
    use anie_protocol::{AssistantMessage, ContentBlock, StopReason, Usage};
    use anie_provider::{
        ApiKind, CostPerMillion, Model, ModelCompat, ProviderError, ProviderRegistry,
        RequestOptionsResolver, ResolvedRequestOptions, ThinkingLevel,
        mock::{MockProvider, MockStreamScript},
    };

    // ---- helpers ----

    fn sample_model() -> Model {
        Model {
            id: "mock-model".into(),
            name: "Mock".into(),
            provider: "mock".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    struct StaticResolver;

    #[async_trait]
    impl RequestOptionsResolver for StaticResolver {
        async fn resolve(
            &self,
            _model: &Model,
            _context: &[Message],
        ) -> Result<ResolvedRequestOptions, ProviderError> {
            Ok(ResolvedRequestOptions::default())
        }
    }

    /// Build a real `AgentLoop` whose provider is a `MockProvider`
    /// scripted to return a single assistant message with the
    /// supplied text. Used by the sub-agent factory in tests so
    /// the recurse tool drives a complete sub-call end-to-end.
    fn build_canned_agent_loop(text: &str) -> AgentLoop {
        let mut provider_registry = ProviderRegistry::new();
        provider_registry.register(
            ApiKind::OpenAICompletions,
            Box::new(MockProvider::new(vec![MockStreamScript::from_message(
                AssistantMessage {
                    content: vec![ContentBlock::Text { text: text.into() }],
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    provider: "mock".into(),
                    model: "mock-model".into(),
                    timestamp: 1,
                    reasoning_details: None,
                },
            )])),
        );
        let config = AgentLoopConfig::new(
            sample_model(),
            "system".into(),
            ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver),
        );
        AgentLoop::new(
            Arc::new(provider_registry),
            Arc::new(ToolRegistry::new()),
            config,
        )
    }

    /// Stub factory that returns a canned `AgentLoop` and
    /// records every build context it was asked to assemble.
    struct StubFactory {
        canned_text: String,
        seen_contexts: Arc<Mutex<Vec<u8>>>, // depths only
    }

    impl SubAgentFactory for StubFactory {
        fn build(&self, ctx: &SubAgentBuildContext) -> anyhow::Result<AgentLoop> {
            self.seen_contexts.lock().unwrap().push(ctx.depth);
            Ok(build_canned_agent_loop(&self.canned_text))
        }
    }

    /// Stub `ContextProvider` that returns canned messages
    /// for any scope, recording the scope it was invoked with.
    struct StubProvider {
        canned: Vec<Message>,
        seen_scopes: Arc<Mutex<Vec<RecurseScope>>>,
    }

    #[async_trait]
    impl ContextProvider for StubProvider {
        async fn resolve(&self, scope: &RecurseScope) -> anyhow::Result<Vec<Message>> {
            self.seen_scopes.lock().unwrap().push(scope.clone());
            Ok(self.canned.clone())
        }
    }

    fn user_message(text: &str) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: 1,
        })
    }

    type SeenContexts = Arc<Mutex<Vec<u8>>>;
    type SeenScopes = Arc<Mutex<Vec<RecurseScope>>>;

    fn build_recurse_tool(
        canned_text: &str,
        canned_messages: Vec<Message>,
        budget: u32,
        depth: u8,
        max_depth: u8,
    ) -> (RecurseTool, SeenContexts, SeenScopes) {
        let seen_contexts = Arc::new(Mutex::new(Vec::new()));
        let seen_scopes = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(StubFactory {
            canned_text: canned_text.into(),
            seen_contexts: Arc::clone(&seen_contexts),
        });
        let provider = Arc::new(StubProvider {
            canned: canned_messages,
            seen_scopes: Arc::clone(&seen_scopes),
        });
        let tool = RecurseTool::new(
            factory,
            provider,
            Arc::new(AtomicU32::new(budget)),
            depth,
            max_depth,
        );
        (tool, seen_contexts, seen_scopes)
    }

    async fn run_tool(
        tool: &RecurseTool,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        tool.execute(
            "test_call",
            args,
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
    }

    fn text_body(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    // ---- tests ----

    /// Definition surface: name + required params present, no
    /// silly typos.
    #[test]
    fn definition_has_recurse_name_and_required_params() {
        let (tool, _, _) = build_recurse_tool("ignored", vec![], 8, 0, 2);
        let def = tool.definition();
        assert_eq!(def.name, "recurse");
        let required = def
            .parameters
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(names.contains(&"query"));
        assert!(names.contains(&"scope"));
    }

    /// End-to-end happy path: the model emits a recurse call,
    /// scope resolves, sub-agent runs, final text is returned.
    /// Locks down every wiring point in one assertion.
    #[tokio::test]
    async fn recurse_drives_subagent_and_returns_final_text() {
        let (tool, seen_contexts, seen_scopes) = build_recurse_tool(
            "the answer is 42",
            vec![user_message("prior context")],
            8,
            0,
            2,
        );
        let result = run_tool(
            &tool,
            serde_json::json!({
                "query": "what is the answer?",
                "scope": {"kind": "message_range", "start": 0, "end": 1}
            }),
        )
        .await
        .expect("recurse ok");
        assert_eq!(text_body(&result), "the answer is 42");
        assert_eq!(result.details["scope_kind"], "message_range");
        assert_eq!(result.details["depth"], 0);
        assert_eq!(result.details["budget_remaining_after"], 7);

        // Factory was asked to build at depth=1 (parent depth 0 + 1).
        assert_eq!(*seen_contexts.lock().unwrap(), vec![1]);
        // Provider saw exactly the scope the model requested.
        assert_eq!(
            *seen_scopes.lock().unwrap(),
            vec![RecurseScope::MessageRange { start: 0, end: 1 }]
        );
    }

    /// Budget exhausted: no factory build, no provider call;
    /// tool returns an `ExecutionFailed` error mentioning
    /// "budget exhausted".
    #[tokio::test]
    async fn recurse_errors_when_budget_exhausted() {
        let (tool, seen_contexts, seen_scopes) = build_recurse_tool("never used", vec![], 0, 0, 2);
        let err = run_tool(
            &tool,
            serde_json::json!({
                "query": "hi",
                "scope": {"kind": "message_range", "start": 0, "end": 0}
            }),
        )
        .await
        .expect_err("budget=0 should error");
        match err {
            ToolError::ExecutionFailed(msg) => {
                assert!(
                    msg.contains("budget exhausted"),
                    "error should name budget exhaustion; got: {msg}",
                );
            }
            other => panic!("expected ExecutionFailed, got: {other:?}"),
        }
        assert!(
            seen_contexts.lock().unwrap().is_empty(),
            "factory must not be invoked when budget is exhausted",
        );
        assert!(
            seen_scopes.lock().unwrap().is_empty(),
            "provider must not be invoked when budget is exhausted",
        );
    }

    /// Budget decrements once per call. Two successful calls
    /// against budget=2 leaves budget=0 and the third errors.
    #[tokio::test]
    async fn recurse_budget_decrements_per_call() {
        let (tool, _, _) = build_recurse_tool("ok", vec![user_message("ctx")], 2, 0, 2);
        let _r1 = run_tool(
            &tool,
            serde_json::json!({"query": "q1", "scope": {"kind": "message_range", "start": 0, "end": 1}}),
        )
        .await
        .expect("call 1 ok");
        let _r2 = run_tool(
            &tool,
            serde_json::json!({"query": "q2", "scope": {"kind": "message_range", "start": 0, "end": 1}}),
        )
        .await
        .expect("call 2 ok");
        let err = run_tool(
            &tool,
            serde_json::json!({"query": "q3", "scope": {"kind": "message_range", "start": 0, "end": 1}}),
        )
        .await
        .expect_err("call 3 should fail");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("budget exhausted")));
    }

    /// Argument parsing rejects malformed JSON cleanly.
    #[tokio::test]
    async fn recurse_rejects_missing_query() {
        let (tool, _, _) = build_recurse_tool("ok", vec![], 8, 0, 2);
        let err = run_tool(
            &tool,
            serde_json::json!({"scope": {"kind": "message_range", "start": 0, "end": 0}}),
        )
        .await
        .expect_err("missing query should error");
        assert!(matches!(err, ToolError::ExecutionFailed(msg) if msg.contains("query")));
    }

    #[tokio::test]
    async fn recurse_rejects_unknown_scope_kind() {
        let (tool, _, _) = build_recurse_tool("ok", vec![], 8, 0, 2);
        let err = run_tool(
            &tool,
            serde_json::json!({"query": "q", "scope": {"kind": "fluffernutter"}}),
        )
        .await
        .expect_err("unknown scope kind should error");
        assert!(
            matches!(err, ToolError::ExecutionFailed(ref msg) if msg.contains("unknown scope kind")),
            "got: {err:?}",
        );
    }

    #[tokio::test]
    async fn recurse_parses_all_four_scope_kinds() {
        // We just confirm the parser accepts each variant
        // without errors. The provider stub returns an empty
        // Vec for any scope, so the sub-agent gets only the
        // sub-query as context — and the canned MockProvider
        // returns a single assistant message regardless.
        for scope_args in [
            serde_json::json!({"kind": "message_range", "start": 0, "end": 0}),
            serde_json::json!({"kind": "message_grep", "pattern": "x"}),
            serde_json::json!({"kind": "tool_result", "tool_call_id": "id"}),
            serde_json::json!({"kind": "file", "path": "/tmp/x"}),
        ] {
            let (tool, _, _) = build_recurse_tool("ok", vec![], 8, 0, 2);
            let r = run_tool(
                &tool,
                serde_json::json!({"query": "q", "scope": scope_args}),
            )
            .await
            .expect("scope parses");
            assert_eq!(text_body(&r), "ok");
        }
    }

    /// Depth field threads correctly: a tool constructed at
    /// depth=N asks the factory to build at depth=N+1.
    #[tokio::test]
    async fn recurse_threads_depth_into_build_context() {
        let (tool, seen_contexts, _) = build_recurse_tool("ok", vec![], 8, 1, 2);
        let _r = run_tool(
            &tool,
            serde_json::json!({"query": "q", "scope": {"kind": "message_range", "start": 0, "end": 0}}),
        )
        .await
        .expect("ok");
        assert_eq!(*seen_contexts.lock().unwrap(), vec![2]);
    }
}
