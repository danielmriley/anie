//! Verifier / self-critique policy.
//!
//! A [`BeforeModelPolicy`] that injects a single, **context-only**
//! critique once per run, nudging the model to verify its plan before
//! continuing. It reads the controller-owned [`TodoList`] so it can gate
//! on *observed* plan state (every item `done`) rather than guessing,
//! with a step-cadence fallback for plan-less runs.
//!
//! Opt-in: enabled only when `ANIE_VERIFIER=1`. Disabled (the default)
//! it returns `Continue`, byte-identical to today. Matches the env-gate
//! discipline of the rlm settings (`ANIE_ACTIVE_CEILING_TOKENS`). See
//! `docs/plan_todo_verifier/README.md` §2.6.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse};
use anie_protocol::{ContentBlock, Message, UserMessage, now_millis};
use anie_tools::TodoList;

/// Default step index at which the plan-less fallback fires. Low enough
/// to land mid-work on a multi-step run, high enough not to nag trivial
/// one-shot answers. Override with `ANIE_VERIFIER_MIN_STEP`.
const DEFAULT_MIN_STEP: u64 = 4;

const CRITIQUE: &str = "Before continuing: review your todo list against what you have actually observed. For each item you marked `done`, confirm you verified it — ran the test, read the file, checked the output. If you cannot verify a claim, mark that item `in_progress` and say what is still needed. Do not declare the task complete on unverified assumptions.";

/// One-shot self-critique policy over the shared plan.
pub(crate) struct VerifierPolicy {
    list: Arc<Mutex<TodoList>>,
    fired: AtomicBool,
    /// Whether the plan trigger is armed: the plan has shown active work
    /// during *this* run, so a later transition to all-done should fire.
    /// Starts `false` only when the policy is constructed over a plan
    /// that is already all-done (a completed plan carried over from a
    /// previous run on the session-lifetime `TodoList`), so a follow-up
    /// run doesn't fire at step 0 before doing any work.
    armed: AtomicBool,
    enabled: bool,
    min_step: u64,
}

impl VerifierPolicy {
    pub(crate) fn new(list: Arc<Mutex<TodoList>>, enabled: bool, min_step: u64) -> Self {
        // A non-empty, already-all-done plan at construction means the
        // plan was completed in a prior run (the list outlives the
        // per-run policy). Start disarmed so we don't critique work the
        // model finished last turn; we re-arm once it does work this run.
        let started_complete = {
            let guard = list.lock().unwrap_or_else(PoisonError::into_inner);
            let (_, total) = guard.counts();
            total > 0 && guard.all_done()
        };
        Self {
            list,
            fired: AtomicBool::new(false),
            armed: AtomicBool::new(!started_complete),
            enabled,
            min_step,
        }
    }

    /// Build from the environment: enabled only when `ANIE_VERIFIER` is
    /// `1`/`true`; `min_step` from `ANIE_VERIFIER_MIN_STEP` else the
    /// default.
    pub(crate) fn from_env(list: Arc<Mutex<TodoList>>) -> Self {
        let enabled = std::env::var("ANIE_VERIFIER")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let min_step = std::env::var("ANIE_VERIFIER_MIN_STEP")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_MIN_STEP);
        Self::new(list, enabled, min_step)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Trigger: if a plan exists, fire when every item is `done` (the
    /// model believes it's finished); otherwise fall back to a step
    /// cadence so plan-less runs still get one critique.
    fn should_fire(&self, step_index: u64) -> bool {
        let guard = self.list.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, total) = guard.counts();
        if total > 0 {
            if !guard.all_done() {
                // Active work this run — arm so a later all-done fires.
                self.armed.store(true, Ordering::Release);
                return false;
            }
            // All done: fire only if the plan actually progressed this
            // run, not if it was already complete at run start.
            self.armed.load(Ordering::Acquire)
        } else {
            step_index >= self.min_step
        }
    }
}

#[async_trait]
impl BeforeModelPolicy for VerifierPolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        if !self.enabled || self.fired.load(Ordering::Acquire) {
            return BeforeModelResponse::Continue;
        }
        if !self.should_fire(request.step_index) {
            return BeforeModelResponse::Continue;
        }
        // One-shot latch: a concurrent caller that lost the race no-ops.
        if self
            .fired
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return BeforeModelResponse::Continue;
        }
        BeforeModelResponse::AppendMessages(vec![critique_message()])
    }
}

fn critique_message() -> Message {
    Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: CRITIQUE.to_string(),
        }],
        timestamp: now_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_tools::{TodoItem, TodoStatus};

    fn list_with(items: Vec<TodoItem>) -> Arc<Mutex<TodoList>> {
        let mut list = TodoList::default();
        list.replace(items);
        Arc::new(Mutex::new(list))
    }

    fn item(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            status,
        }
    }

    fn request(step_index: u64) -> BeforeModelRequest<'static> {
        // Empty borrows that live for 'static via leaked-free statics.
        static EMPTY: &[Message] = &[];
        BeforeModelRequest {
            context: EMPTY,
            generated_messages: EMPTY,
            model: model(),
            step_index,
            run_usage: empty_usage(),
        }
    }

    fn empty_usage() -> &'static anie_protocol::Usage {
        use std::sync::OnceLock;
        static USAGE: OnceLock<anie_protocol::Usage> = OnceLock::new();
        USAGE.get_or_init(anie_protocol::Usage::default)
    }

    fn model() -> &'static anie_provider::Model {
        use std::sync::OnceLock;
        static MODEL: OnceLock<anie_provider::Model> = OnceLock::new();
        MODEL.get_or_init(|| anie_provider::Model {
            id: "m".into(),
            name: "m".into(),
            provider: "p".into(),
            api: anie_provider::ApiKind::OpenAICompletions,
            base_url: "http://localhost".into(),
            context_window: 1000,
            max_tokens: 100,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: anie_provider::CostPerMillion::zero(),
            replay_capabilities: None,
            compat: anie_provider::ModelCompat::None,
        })
    }

    #[tokio::test]
    async fn verifier_disabled_returns_continue() {
        let list = list_with(vec![item("a", TodoStatus::Done)]);
        let policy = VerifierPolicy::new(list, false, DEFAULT_MIN_STEP);
        assert_eq!(
            policy.before_model(request(0)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn verifier_fires_once_when_plan_becomes_all_done_during_run() {
        // The plan starts with active work (so the trigger is armed),
        // then the model marks everything done this run.
        let list = list_with(vec![
            item("a", TodoStatus::Pending),
            item("b", TodoStatus::InProgress),
        ]);
        let policy = VerifierPolicy::new(Arc::clone(&list), true, DEFAULT_MIN_STEP);

        // Not done yet — no fire (and this arms the trigger).
        assert_eq!(
            policy.before_model(request(0)).await,
            BeforeModelResponse::Continue
        );

        // Model completes the plan this run.
        list.lock().unwrap().replace(vec![
            item("a", TodoStatus::Done),
            item("b", TodoStatus::Done),
        ]);

        // Now it fires once with an appended critique.
        match policy.before_model(request(1)).await {
            BeforeModelResponse::AppendMessages(messages) => {
                assert_eq!(messages.len(), 1);
                assert!(matches!(&messages[0], Message::User(_)));
            }
            other => panic!("expected AppendMessages, got {other:?}"),
        }
        // Second call is a no-op (one-shot latch).
        assert_eq!(
            policy.before_model(request(2)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn verifier_does_not_fire_on_carried_over_all_done_plan() {
        // A follow-up run: the policy is reconstructed, but the
        // session-lifetime plan is already all-done from the previous
        // run. It must NOT critique at step 0 before any work this run.
        let list = list_with(vec![
            item("a", TodoStatus::Done),
            item("b", TodoStatus::Done),
        ]);
        let policy = VerifierPolicy::new(list, true, DEFAULT_MIN_STEP);
        assert_eq!(
            policy.before_model(request(0)).await,
            BeforeModelResponse::Continue
        );
        assert_eq!(
            policy.before_model(request(1)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn verifier_does_not_fire_while_plan_incomplete() {
        let list = list_with(vec![
            item("a", TodoStatus::Done),
            item("b", TodoStatus::Pending),
        ]);
        let policy = VerifierPolicy::new(list, true, DEFAULT_MIN_STEP);
        // A plan exists but isn't all done — no nag yet, even past the
        // step cadence (the plan trigger takes precedence).
        assert_eq!(
            policy.before_model(request(99)).await,
            BeforeModelResponse::Continue
        );
    }

    #[tokio::test]
    async fn verifier_step_cadence_fallback_fires_without_todos() {
        // No plan was written: fall back to the step cadence.
        let list = Arc::new(Mutex::new(TodoList::default()));
        let policy = VerifierPolicy::new(list, true, 4);
        // Below the threshold: nothing.
        assert_eq!(
            policy.before_model(request(3)).await,
            BeforeModelResponse::Continue
        );
        // At/above the threshold: fires once.
        assert!(matches!(
            policy.before_model(request(4)).await,
            BeforeModelResponse::AppendMessages(_)
        ));
    }

    // ---- end-to-end: plan -> verifier critique, context-only ----

    use anie_agent::{AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry};
    use anie_protocol::{AssistantMessage, StopReason, ToolCall, Usage};
    use anie_provider::{
        ApiKind, ProviderError, ProviderRegistry, RequestOptionsResolver, ResolvedRequestOptions,
        mock::{MockProvider, MockStreamScript},
    };
    use async_trait::async_trait;

    struct StaticResolver;

    #[async_trait]
    impl RequestOptionsResolver for StaticResolver {
        async fn resolve(
            &self,
            _model: &anie_provider::Model,
            _context: &[Message],
        ) -> Result<ResolvedRequestOptions, ProviderError> {
            Ok(ResolvedRequestOptions::default())
        }
    }

    fn assistant_tool_call(id: &str, args: serde_json::Value) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.into(),
                name: "todo_write".into(),
                arguments: args,
            })],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "mock".into(),
            model: "m".into(),
            timestamp: 1,
            reasoning_details: None,
        }
    }

    fn assistant_done() -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "all set".into(),
            }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "mock".into(),
            model: "m".into(),
            timestamp: 1,
            reasoning_details: None,
        }
    }

    fn contains_critique(messages: &[Message]) -> bool {
        messages.iter().any(|message| match message {
            Message::User(u) => u
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == CRITIQUE)),
            _ => false,
        })
    }

    #[tokio::test]
    async fn todo_plan_drives_verifier_critique_end_to_end() {
        // The model writes an all-done plan via todo_write on turn 1,
        // then answers on turn 2. With the verifier enabled, the critique
        // must land in the run context before turn 2 but NOT in the
        // controller-persisted generated_messages (context-only).
        let list = Arc::new(Mutex::new(TodoList::default()));

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(anie_tools::TodoWriteTool::new(Arc::clone(&list))));

        let mut providers = ProviderRegistry::new();
        providers.register(
            ApiKind::OpenAICompletions,
            Box::new(MockProvider::new(vec![
                MockStreamScript::from_message(assistant_tool_call(
                    "c1",
                    serde_json::json!({ "todos": [{ "content": "do it", "status": "done" }] }),
                )),
                MockStreamScript::from_message(assistant_done()),
            ])),
        );

        let config = AgentLoopConfig::new(
            model().clone(),
            "system".into(),
            anie_provider::ThinkingLevel::Off,
            ToolExecutionMode::Sequential,
            Arc::new(StaticResolver),
        )
        .with_before_model_policy(Arc::new(VerifierPolicy::new(
            Arc::clone(&list),
            true,
            4,
        )));

        let agent = AgentLoop::new(Arc::new(providers), Arc::new(tools), config);
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let result = agent
            .run(
                vec![Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: "please do the thing".into(),
                    }],
                    timestamp: 1,
                })],
                Vec::new(),
                tx,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;

        assert!(
            result.terminal_error.is_none(),
            "run should complete: {:?}",
            result.terminal_error
        );
        // The plan was actually written through the shared list.
        assert!(list.lock().expect("lock").all_done());
        // Critique is in the canonical context...
        assert!(
            contains_critique(&result.final_context),
            "critique must be injected into the run context"
        );
        // ...but NOT persisted as agent output.
        assert!(
            !contains_critique(&result.generated_messages),
            "critique is context-only and must not appear in generated_messages"
        );
    }
}
