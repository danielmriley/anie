use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::{StreamExt, future::join_all};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Process-global "have we already warned about a closed AgentEvent
/// channel?" flag. First failure per process lifetime logs a
/// warning; every subsequent drop is silent. Not reset between
/// runs — a receiver-gone condition is interesting once, not per
/// event.
static EVENT_DROP_WARNED: AtomicBool = AtomicBool::new(false);

/// Send an `AgentEvent` to the UI/controller, warning once per
/// process lifetime when the receiver has dropped. Replaces the
/// `let _ = tx.send(...).await` pattern so silent channel closure
/// becomes visible without flooding logs. Shared across agent_loop
/// and the CLI controller so both sides trip the same latch.
pub async fn send_event(tx: &mpsc::Sender<AgentEvent>, event: AgentEvent) {
    if tx.send(event).await.is_err() && !EVENT_DROP_WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "agent event channel closed; subsequent events in this run will be dropped \
             silently (the consumer has likely exited)"
        );
    }
}

#[cfg(test)]
mod send_event_tests {
    use std::{
        io,
        sync::{Arc, Mutex, OnceLock},
    };

    use tokio::sync::Mutex as AsyncMutex;
    use tracing_subscriber::{
        fmt::{self, writer::MakeWriter},
        layer::SubscriberExt,
    };

    use super::*;

    static SEND_EVENT_TEST_GUARD: OnceLock<AsyncMutex<()>> = OnceLock::new();

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl<'a> MakeWriter<'a> for LogCapture {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            LogWriter(Arc::clone(&self.0))
        }
    }

    impl io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl LogCapture {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("log lock").clone()).expect("utf8 logs")
        }
    }

    fn test_guard() -> &'static AsyncMutex<()> {
        SEND_EVENT_TEST_GUARD.get_or_init(|| AsyncMutex::new(()))
    }

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.match_indices(needle).count()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_event_logs_once_when_channel_closed() {
        let _guard = test_guard().lock().await;
        EVENT_DROP_WARNED.store(false, Ordering::Relaxed);

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .with_writer(capture.clone()),
        );
        let _default = tracing::subscriber::set_default(subscriber);

        let (tx, rx) = mpsc::channel::<AgentEvent>(1);
        drop(rx);

        send_event(
            &tx,
            AgentEvent::SystemMessage {
                text: "first".into(),
            },
        )
        .await;
        send_event(
            &tx,
            AgentEvent::SystemMessage {
                text: "second".into(),
            },
        )
        .await;
        send_event(
            &tx,
            AgentEvent::SystemMessage {
                text: "third".into(),
            },
        )
        .await;

        let logs = capture.contents();
        assert_eq!(count_occurrences(&logs, "agent event channel closed"), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_event_does_not_log_on_first_success() {
        let _guard = test_guard().lock().await;
        EVENT_DROP_WARNED.store(false, Ordering::Relaxed);

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .with_writer(capture.clone()),
        );
        let _default = tracing::subscriber::set_default(subscriber);

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(4);
        send_event(&tx, AgentEvent::SystemMessage { text: "ok".into() }).await;

        assert_eq!(
            count_occurrences(&capture.contents(), "agent event channel closed"),
            0
        );
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::SystemMessage { text }) if text == "ok"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn send_event_latch_is_process_global() {
        let _guard = test_guard().lock().await;
        EVENT_DROP_WARNED.store(false, Ordering::Relaxed);

        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .with_writer(capture.clone()),
        );
        let _default = tracing::subscriber::set_default(subscriber);

        let (tx1, rx1) = mpsc::channel::<AgentEvent>(1);
        let (tx2, rx2) = mpsc::channel::<AgentEvent>(1);
        drop(rx1);
        drop(rx2);

        send_event(&tx1, AgentEvent::SystemMessage { text: "a".into() }).await;
        send_event(&tx2, AgentEvent::SystemMessage { text: "b".into() }).await;

        let logs = capture.contents();
        assert_eq!(count_occurrences(&logs, "agent event channel closed"), 1);
    }
}

use anie_protocol::{
    AgentEvent, AssistantMessage, ContentBlock, Message, StopReason, StreamDelta, ToolCall,
    ToolResult, ToolResultMessage, now_millis,
};
use anie_provider::{
    LlmContext, Model, ProviderError, ProviderEvent, ProviderRegistry, ProviderStream,
    RequestOptionsResolver, StreamOptions, ThinkingLevel,
};

use crate::ToolRegistry;
use crate::hooks::{
    AfterToolCallHook, BeforeToolCallHook, BeforeToolCallResult, ToolResultOverride,
};
use crate::tool::ToolExecutionContext;

/// Agent-loop execution mode for tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionMode {
    /// Run one tool at a time.
    Sequential,
    /// Run all tool calls for the turn concurrently.
    Parallel,
}

/// Outcome from a `CompactionGate::maybe_compact` call.
///
/// Plan `docs/midturn_compaction_2026-04-27/03_agent_loop_compaction_signal.md`.
/// PR 8.3 — the trait and enum land default-off; plan 04 (PR
/// 8.4) installs a real implementation in the controller.
#[derive(Debug, Clone)]
pub enum CompactionGateOutcome {
    /// No compaction needed; the loop continues with the same
    /// context.
    Continue,
    /// Compaction ran; replace the loop's context with `messages`.
    /// The agent loop emits `AgentEvent::TranscriptReplace`
    /// before the next sampling iteration so the UI re-renders
    /// from the post-compaction transcript.
    Compacted {
        /// Replacement context. Must preserve any in-flight
        /// tool-call correlation (the existing pre-prompt
        /// compaction primitive `find_cut_point` is the model
        /// for this).
        messages: Vec<Message>,
    },
    /// Gate decided not to compact this time (typically because
    /// the per-turn budget is exhausted, but the trait is
    /// reason-agnostic). The loop emits a `SystemMessage` with
    /// the supplied reason and continues; the next sampling
    /// request may still overflow, in which case the reactive
    /// retry path handles it.
    Skipped {
        /// Human-readable reason surfaced to the user / logs.
        reason: String,
    },
}

/// Hook invoked between agent-loop iterations to decide
/// whether mid-turn compaction is warranted.
///
/// Called after each sampling response has been merged into
/// the loop's context (assistant message + any tool results +
/// any steering messages) and *before* the next sampling
/// iteration's request is built. That timing matches codex's
/// pattern (`codex-rs/core/src/codex.rs:6420-6468`) and gives
/// the controller a chance to shrink context before the agent
/// burns another sampling request on a too-large prompt.
///
/// **Default behavior:** `AgentLoopConfig::compaction_gate`
/// defaults to `None`, in which case the loop never invokes
/// this hook — no behavior change for callers that don't
/// install one.
///
/// Plan `docs/midturn_compaction_2026-04-27/03_agent_loop_compaction_signal.md`.
#[async_trait::async_trait]
pub trait CompactionGate: Send + Sync {
    /// Inspect `context` and decide whether to compact, leave
    /// it unchanged, or explicitly skip with a reason.
    async fn maybe_compact(
        &self,
        context: &[Message],
    ) -> Result<CompactionGateOutcome, anyhow::Error>;
}

/// Immutable configuration for an agent loop instance.
pub struct AgentLoopConfig {
    model: Model,
    system_prompt: String,
    thinking: ThinkingLevel,
    tool_execution: ToolExecutionMode,
    request_options_resolver: Arc<dyn RequestOptionsResolver>,
    ollama_num_ctx_override: Option<u64>,
    get_steering_messages: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    get_follow_up_messages: Option<Arc<dyn Fn() -> Vec<Message> + Send + Sync>>,
    before_tool_call_hook: Option<Arc<dyn BeforeToolCallHook>>,
    after_tool_call_hook: Option<Arc<dyn AfterToolCallHook>>,
    /// Optional mid-turn compaction hook. Default `None`. Plan
    /// 8.3 of `docs/midturn_compaction_2026-04-27/`. The
    /// controller installs a real implementation in plan 8.4;
    /// today's call sites all pass `None`, so this is a pure
    /// plumbing change with zero behavioral effect.
    compaction_gate: Option<Arc<dyn CompactionGate>>,
}

impl AgentLoopConfig {
    /// Create a config with no steering/follow-up/tool hooks.
    #[must_use]
    pub fn new(
        model: Model,
        system_prompt: String,
        thinking: ThinkingLevel,
        tool_execution: ToolExecutionMode,
        request_options_resolver: Arc<dyn RequestOptionsResolver>,
    ) -> Self {
        Self {
            model,
            system_prompt,
            thinking,
            tool_execution,
            request_options_resolver,
            ollama_num_ctx_override: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call_hook: None,
            after_tool_call_hook: None,
            compaction_gate: None,
        }
    }

    /// Install a [`CompactionGate`] that the loop consults at
    /// the bottom of each iteration (after the first).
    /// Defaults to `None`. PR 8.3 of
    /// `docs/midturn_compaction_2026-04-27/`.
    #[must_use]
    pub fn with_compaction_gate(mut self, gate: Option<Arc<dyn CompactionGate>>) -> Self {
        self.compaction_gate = gate;
        self
    }

    /// Snapshot an Ollama native `num_ctx` override for provider
    /// requests. Non-Ollama providers ignore the resulting
    /// `StreamOptions` field.
    #[must_use]
    pub fn with_ollama_num_ctx_override(mut self, override_value: Option<u64>) -> Self {
        self.ollama_num_ctx_override = override_value;
        self
    }

    fn stream_options(
        &self,
        api_key: Option<String>,
        headers: HashMap<String, String>,
    ) -> StreamOptions {
        StreamOptions {
            api_key,
            temperature: None,
            // Intentionally `None`: the upstream owns the
            // `input + output <= context_window` invariant
            // server-side and knows the real tokenizer.
            // Bounding output at the agent layer is how pi avoids
            // context-overflow 400s
            // (`pi/packages/ai/src/providers/openai-completions.ts:394`
            // only emits `max_tokens` when the caller explicitly
            // sets it). Budget management is handled by compaction
            // (`reserve_tokens`), which shrinks the *input* instead
            // of capping the *output*. Compaction summarization still
            // sets its own `max_tokens` explicitly — see
            // `anie-cli/src/compaction.rs`.
            max_tokens: None,
            thinking: self.thinking,
            headers,
            num_ctx_override: self.ollama_num_ctx_override,
        }
    }

    /// Attach a per-turn steering-message provider.
    #[must_use]
    pub fn with_steering_messages(
        mut self,
        get_steering_messages: Arc<dyn Fn() -> Vec<Message> + Send + Sync>,
    ) -> Self {
        self.get_steering_messages = Some(get_steering_messages);
        self
    }

    /// Attach a post-tool follow-up-message provider.
    #[must_use]
    pub fn with_follow_up_messages(
        mut self,
        get_follow_up_messages: Arc<dyn Fn() -> Vec<Message> + Send + Sync>,
    ) -> Self {
        self.get_follow_up_messages = Some(get_follow_up_messages);
        self
    }

    /// Attach internal tool-execution hooks.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_hooks(
        mut self,
        before_tool_call_hook: Option<Arc<dyn BeforeToolCallHook>>,
        after_tool_call_hook: Option<Arc<dyn AfterToolCallHook>>,
    ) -> Self {
        self.before_tool_call_hook = before_tool_call_hook;
        self.after_tool_call_hook = after_tool_call_hook;
        self
    }
}

/// Final agent-run output.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentRunResult {
    /// Messages generated by the assistant/tool loop (not including prompts).
    pub generated_messages: Vec<Message>,
    /// Final canonical context including prompts and generated messages.
    pub final_context: Vec<Message>,
    /// Structured terminal provider error, if the run ended with one.
    pub terminal_error: Option<ProviderError>,
}

/// The core provider/tool-agnostic agent loop.
pub struct AgentLoop {
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    config: AgentLoopConfig,
}

impl AgentLoop {
    /// Create a new agent loop.
    #[must_use]
    pub fn new(
        provider_registry: Arc<ProviderRegistry>,
        tool_registry: Arc<ToolRegistry>,
        config: AgentLoopConfig,
    ) -> Self {
        Self {
            provider_registry,
            tool_registry,
            config,
        }
    }

    /// Run the agent loop using owned prompt and context state.
    pub async fn run(
        &self,
        prompts: Vec<Message>,
        context: Vec<Message>,
        event_tx: mpsc::Sender<AgentEvent>,
        cancel: CancellationToken,
    ) -> AgentRunResult {
        let mut context = context;
        let mut generated_messages = Vec::new();
        context.extend(prompts.iter().cloned());
        // Reborrow event_tx as a shared reference so every call site
        // below and the helper methods (which already take `&`) share
        // the same shape. `.clone()` on `&Sender` still works where we
        // need to move a sender into a spawned task.
        let event_tx = &event_tx;

        send_event(event_tx, AgentEvent::AgentStart).await;
        send_event(event_tx, AgentEvent::TurnStart).await;

        for prompt in &prompts {
            send_event(
                event_tx,
                AgentEvent::MessageStart {
                    message: prompt.clone(),
                },
            )
            .await;
            send_event(
                event_tx,
                AgentEvent::MessageEnd {
                    message: prompt.clone(),
                },
            )
            .await;
        }

        loop {
            let request = match self
                .config
                .request_options_resolver
                .resolve(&self.config.model, &context)
                .await
            {
                Ok(request) => request,
                Err(error) => {
                    let assistant =
                        self.error_assistant_message(error.to_string(), StopReason::Error);
                    self.finish_with_assistant(
                        assistant,
                        &mut context,
                        &mut generated_messages,
                        event_tx,
                        Vec::new(),
                    )
                    .await;
                    return AgentRunResult {
                        generated_messages,
                        final_context: context,
                        terminal_error: Some(error),
                    };
                }
            };

            let Some(provider) = self.provider_registry.get(&self.config.model.api) else {
                let assistant = self.error_assistant_message(
                    format!("No provider registered for {:?}", self.config.model.api),
                    StopReason::Error,
                );
                self.finish_with_assistant(
                    assistant,
                    &mut context,
                    &mut generated_messages,
                    event_tx,
                    Vec::new(),
                )
                .await;
                return AgentRunResult {
                    generated_messages,
                    final_context: context,
                    terminal_error: None,
                };
            };

            let mut model = self.config.model.clone();
            if let Some(base_url_override) = request.base_url_override {
                model.base_url = base_url_override;
            }

            let replay = model.effective_replay_capabilities();
            let sanitized_context = sanitize_context_for_request(
                &context,
                provider.includes_thinking_in_replay(),
                replay.requires_thinking_signature,
            );
            let llm_context = LlmContext {
                system_prompt: self.config.system_prompt.clone(),
                messages: provider.convert_messages(&sanitized_context),
                tools: self.tool_registry.definitions(),
            };
            let options = self.config.stream_options(request.api_key, request.headers);

            let stream = match provider.stream(&model, llm_context, options) {
                Ok(stream) => stream,
                Err(error) => {
                    let assistant =
                        self.error_assistant_message(error.to_string(), StopReason::Error);
                    self.finish_with_assistant(
                        assistant,
                        &mut context,
                        &mut generated_messages,
                        event_tx,
                        Vec::new(),
                    )
                    .await;
                    return AgentRunResult {
                        generated_messages,
                        final_context: context,
                        terminal_error: Some(error),
                    };
                }
            };

            let collected = self.collect_stream(stream, event_tx, &cancel).await;
            let assistant = collected.assistant;
            let assistant_message = Message::Assistant(assistant.clone());
            context.push(assistant_message.clone());
            generated_messages.push(assistant_message);

            if collected.provider_error.is_some()
                || matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                )
            {
                send_event(
                    event_tx,
                    AgentEvent::TurnEnd {
                        assistant,
                        tool_results: Vec::new(),
                    },
                )
                .await;
                send_event(
                    event_tx,
                    AgentEvent::AgentEnd {
                        messages: generated_messages.clone(),
                    },
                )
                .await;
                return AgentRunResult {
                    generated_messages,
                    final_context: context,
                    terminal_error: collected.provider_error,
                };
            }

            let tool_calls = extract_tool_calls(&assistant);
            if tool_calls.is_empty() {
                if let Some(get_follow_up_messages) = &self.config.get_follow_up_messages {
                    let follow_up_messages = get_follow_up_messages();
                    if !follow_up_messages.is_empty() {
                        context.extend(follow_up_messages);
                        send_event(
                            event_tx,
                            AgentEvent::TurnEnd {
                                assistant,
                                tool_results: Vec::new(),
                            },
                        )
                        .await;
                        send_event(event_tx, AgentEvent::TurnStart).await;
                        continue;
                    }
                }

                send_event(
                    event_tx,
                    AgentEvent::TurnEnd {
                        assistant,
                        tool_results: Vec::new(),
                    },
                )
                .await;
                send_event(
                    event_tx,
                    AgentEvent::AgentEnd {
                        messages: generated_messages.clone(),
                    },
                )
                .await;
                return AgentRunResult {
                    generated_messages,
                    final_context: context,
                    terminal_error: None,
                };
            }

            let tool_results = self
                .execute_tool_calls(&tool_calls, &context, event_tx, &cancel)
                .await;

            for tool_result in &tool_results {
                let message = Message::ToolResult(tool_result.clone());
                context.push(message.clone());
                generated_messages.push(message);
            }

            if let Some(get_steering_messages) = &self.config.get_steering_messages {
                context.extend(get_steering_messages());
            }

            send_event(
                event_tx,
                AgentEvent::TurnEnd {
                    assistant,
                    tool_results: tool_results.clone(),
                },
            )
            .await;

            if cancel.is_cancelled() {
                send_event(
                    event_tx,
                    AgentEvent::AgentEnd {
                        messages: generated_messages.clone(),
                    },
                )
                .await;
                return AgentRunResult {
                    generated_messages,
                    final_context: context,
                    terminal_error: None,
                };
            }

            // Mid-turn compaction gate. PR 8.3 of
            // `docs/midturn_compaction_2026-04-27/`. Fires AFTER
            // tool results / steering messages have been merged
            // into `context` and BEFORE the next sampling
            // iteration starts. The first iteration is unaffected
            // because we never reach this point without having
            // completed at least one full sampling cycle. Default
            // `None` makes this a single `Option::is_some` branch
            // — zero cost for callers that don't install a gate.
            if let Some(gate) = self.config.compaction_gate.as_ref() {
                match gate.maybe_compact(&context).await {
                    Ok(CompactionGateOutcome::Continue) => {}
                    Ok(CompactionGateOutcome::Compacted { messages }) => {
                        context = messages;
                        send_event(
                            event_tx,
                            AgentEvent::TranscriptReplace {
                                messages: context.clone(),
                            },
                        )
                        .await;
                    }
                    Ok(CompactionGateOutcome::Skipped { reason }) => {
                        send_event(
                            event_tx,
                            AgentEvent::SystemMessage {
                                text: format!("Skipped mid-turn compaction: {reason}"),
                            },
                        )
                        .await;
                    }
                    Err(error) => {
                        // A gate failure is non-fatal: the next
                        // sampling request may still overflow, and
                        // the reactive retry path will handle it.
                        // Surfacing as a warn keeps the failure
                        // observable without killing an in-flight
                        // turn.
                        warn!(?error, "compaction gate failed");
                    }
                }
            }

            send_event(event_tx, AgentEvent::TurnStart).await;
        }
    }

    async fn finish_with_assistant(
        &self,
        assistant: AssistantMessage,
        context: &mut Vec<Message>,
        generated_messages: &mut Vec<Message>,
        event_tx: &mpsc::Sender<AgentEvent>,
        tool_results: Vec<ToolResultMessage>,
    ) {
        let message = Message::Assistant(assistant.clone());
        send_event(
            event_tx,
            AgentEvent::MessageStart {
                message: message.clone(),
            },
        )
        .await;
        send_event(
            event_tx,
            AgentEvent::MessageEnd {
                message: message.clone(),
            },
        )
        .await;
        context.push(message.clone());
        generated_messages.push(message);
        send_event(
            event_tx,
            AgentEvent::TurnEnd {
                assistant,
                tool_results,
            },
        )
        .await;
        send_event(
            event_tx,
            AgentEvent::AgentEnd {
                messages: generated_messages.clone(),
            },
        )
        .await;
    }

    async fn collect_stream(
        &self,
        stream: ProviderStream,
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> CollectedAssistant {
        let mut builder = AssistantMessageBuilder::new(
            self.config.model.provider.clone(),
            self.config.model.id.clone(),
        );
        let placeholder = Message::Assistant(builder.placeholder_message());
        send_event(
            event_tx,
            AgentEvent::MessageStart {
                message: placeholder,
            },
        )
        .await;

        tokio::pin!(stream);
        let mut active_delta = None;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    self.finish_active_delta(event_tx, &mut active_delta).await;
                    let assistant = builder.finish(StopReason::Aborted, Some("Run aborted".into()));
                    send_event(event_tx, AgentEvent::MessageEnd { message: Message::Assistant(assistant.clone()) }).await;
                    return CollectedAssistant {
                        assistant,
                        provider_error: None,
                    };
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(provider_event)) => match provider_event {
                            ProviderEvent::Start => {}
                            ProviderEvent::TextDelta(text) => {
                                self.start_delta_if_needed(event_tx, &mut active_delta, ActiveDelta::Text).await;
                                builder.push_text(&text);
                                send_event(event_tx, AgentEvent::MessageDelta { delta: StreamDelta::TextDelta(text) }).await;
                            }
                            ProviderEvent::ThinkingDelta(thinking) => {
                                self.start_delta_if_needed(event_tx, &mut active_delta, ActiveDelta::Thinking).await;
                                builder.push_thinking(&thinking);
                                send_event(event_tx, AgentEvent::MessageDelta { delta: StreamDelta::ThinkingDelta(thinking) }).await;
                            }
                            ProviderEvent::ToolCallStart(tool_call) => {
                                self.finish_active_delta(event_tx, &mut active_delta).await;
                                builder.start_tool_call(tool_call.clone());
                                send_event(event_tx, AgentEvent::MessageDelta { delta: StreamDelta::ToolCallStart(tool_call) }).await;
                            }
                            ProviderEvent::ToolCallDelta { id, arguments_delta } => {
                                builder.append_tool_call_delta(&id, &arguments_delta);
                                send_event(event_tx, AgentEvent::MessageDelta {
                                    delta: StreamDelta::ToolCallDelta {
                                        id,
                                        arguments_delta,
                                    },
                                }).await;
                            }
                            ProviderEvent::ToolCallEnd { id } => {
                                builder.finish_tool_call(&id);
                                send_event(event_tx, AgentEvent::MessageDelta {
                                    delta: StreamDelta::ToolCallEnd { id },
                                }).await;
                            }
                            ProviderEvent::Done(message) => {
                                self.finish_active_delta(event_tx, &mut active_delta).await;
                                send_event(event_tx, AgentEvent::MessageEnd { message: Message::Assistant(message.clone()) }).await;
                                return CollectedAssistant {
                                    assistant: message,
                                    provider_error: None,
                                };
                            }
                        },
                        Some(Err(error)) => {
                            self.finish_active_delta(event_tx, &mut active_delta).await;
                            let assistant = builder.finish(StopReason::Error, Some(error.to_string()));
                            send_event(event_tx, AgentEvent::MessageEnd { message: Message::Assistant(assistant.clone()) }).await;
                            return CollectedAssistant {
                                assistant,
                                provider_error: Some(error),
                            };
                        }
                        None => {
                            self.finish_active_delta(event_tx, &mut active_delta).await;
                            let error = ProviderError::MalformedStreamEvent(
                                "Stream ended unexpectedly".into(),
                            );
                            let assistant = builder.finish(StopReason::Error, Some(error.to_string()));
                            send_event(event_tx, AgentEvent::MessageEnd { message: Message::Assistant(assistant.clone()) }).await;
                            return CollectedAssistant {
                                assistant,
                                provider_error: Some(error),
                            };
                        }
                    }
                }
            }
        }
    }

    async fn start_delta_if_needed(
        &self,
        event_tx: &mpsc::Sender<AgentEvent>,
        active_delta: &mut Option<ActiveDelta>,
        next: ActiveDelta,
    ) {
        if *active_delta == Some(next) {
            return;
        }

        self.finish_active_delta(event_tx, active_delta).await;
        let delta = match next {
            ActiveDelta::Text => StreamDelta::TextStart,
            ActiveDelta::Thinking => StreamDelta::ThinkingStart,
        };
        send_event(event_tx, AgentEvent::MessageDelta { delta }).await;
        *active_delta = Some(next);
    }

    async fn finish_active_delta(
        &self,
        event_tx: &mpsc::Sender<AgentEvent>,
        active_delta: &mut Option<ActiveDelta>,
    ) {
        if let Some(delta_kind) = active_delta.take() {
            let delta = match delta_kind {
                ActiveDelta::Text => StreamDelta::TextEnd,
                ActiveDelta::Thinking => StreamDelta::ThinkingEnd,
            };
            send_event(event_tx, AgentEvent::MessageDelta { delta }).await;
        }
    }

    async fn execute_tool_calls(
        &self,
        tool_calls: &[ToolCall],
        context: &[Message],
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> Vec<ToolResultMessage> {
        match self.config.tool_execution {
            ToolExecutionMode::Sequential => {
                let mut results = Vec::with_capacity(tool_calls.len());
                for tool_call in tool_calls {
                    results.push(
                        self.execute_single_tool(tool_call.clone(), context, event_tx, cancel)
                            .await,
                    );
                }
                results
            }
            ToolExecutionMode::Parallel => {
                let futures = tool_calls.iter().cloned().map(|tool_call| {
                    self.execute_single_tool(tool_call, context, event_tx, cancel)
                });
                join_all(futures).await
            }
        }
    }

    async fn execute_single_tool(
        &self,
        tool_call: ToolCall,
        context: &[Message],
        event_tx: &mpsc::Sender<AgentEvent>,
        cancel: &CancellationToken,
    ) -> ToolResultMessage {
        send_event(
            event_tx,
            AgentEvent::ToolExecStart {
                call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let Some(tool) = self.tool_registry.get(&tool_call.name) else {
            let result = error_tool_result(format!("Tool not found: {}", tool_call.name));
            send_event(
                event_tx,
                AgentEvent::ToolExecEnd {
                    call_id: tool_call.id.clone(),
                    result: result.clone(),
                    is_error: true,
                },
            )
            .await;
            return tool_result_message(&tool_call, result, true);
        };

        // Fetch the precompiled validator from the registry.
        // Missing validator would mean we skipped registration
        // (can't happen here — `tool_registry.get` above
        // returned Some(tool) so the validator also exists),
        // but handle it defensively with the legacy error
        // message so behavior is identical to the pre-cache
        // code path.
        let validator_state = self.tool_registry.validator(&tool_call.name);
        let validation_result = match validator_state {
            Some(state) => validate_tool_arguments(state, &tool_call.arguments),
            None => Err("Tool schema compilation failed: validator missing from registry".into()),
        };
        if let Err(message) = validation_result {
            let result = error_tool_result(message);
            send_event(
                event_tx,
                AgentEvent::ToolExecEnd {
                    call_id: tool_call.id.clone(),
                    result: result.clone(),
                    is_error: true,
                },
            )
            .await;
            return tool_result_message(&tool_call, result, true);
        }

        if let Some(hook) = &self.config.before_tool_call_hook {
            match hook
                .before_tool_call(&tool_call, &tool_call.arguments, context)
                .await
            {
                BeforeToolCallResult::Allow => {}
                BeforeToolCallResult::Block { reason } => {
                    let result = error_tool_result(reason);
                    send_event(
                        event_tx,
                        AgentEvent::ToolExecEnd {
                            call_id: tool_call.id.clone(),
                            result: result.clone(),
                            is_error: true,
                        },
                    )
                    .await;
                    return tool_result_message(&tool_call, result, true);
                }
            }
        }

        let (update_tx, mut update_rx) = mpsc::channel(16);
        let update_call_id = tool_call.id.clone();
        let update_event_tx = event_tx.clone();
        let update_forwarder = tokio::spawn(async move {
            while let Some(partial) = update_rx.recv().await {
                let _ = update_event_tx
                    .send(AgentEvent::ToolExecUpdate {
                        call_id: update_call_id.clone(),
                        partial,
                    })
                    .await;
            }
        });

        let tool_ctx = self.tool_execution_context();
        let execution = tool
            .execute(
                &tool_call.id,
                tool_call.arguments.clone(),
                cancel.child_token(),
                Some(update_tx),
                &tool_ctx,
            )
            .await;
        let _ = update_forwarder.await;

        let (mut result, mut is_error) = match execution {
            Ok(result) => (result, false),
            Err(error) => (error_tool_result(error.to_string()), true),
        };

        if let Some(hook) = &self.config.after_tool_call_hook {
            if let Some(override_result) = hook.after_tool_call(&tool_call, &result, is_error).await
            {
                apply_tool_result_override(&mut result, &mut is_error, override_result);
            }
        }

        attach_tool_invocation_details(&tool_call, &mut result.details);

        send_event(
            event_tx,
            AgentEvent::ToolExecEnd {
                call_id: tool_call.id.clone(),
                result: result.clone(),
                is_error,
            },
        )
        .await;

        tool_result_message(&tool_call, result, is_error)
    }

    /// Build the per-execution tool context for this run.
    ///
    /// `context_window` honors a runtime `num_ctx` override
    /// (set by `/context-length` for Ollama models) and falls
    /// back to the catalog `Model::context_window`. Plan 05 of
    /// `docs/midturn_compaction_2026-04-27/`.
    fn tool_execution_context(&self) -> ToolExecutionContext {
        let context_window = self
            .config
            .ollama_num_ctx_override
            .unwrap_or(self.config.model.context_window);
        ToolExecutionContext { context_window }
    }

    fn error_assistant_message(
        &self,
        error_message: String,
        stop_reason: StopReason,
    ) -> AssistantMessage {
        AssistantMessage {
            content: vec![ContentBlock::Text {
                text: error_message.clone(),
            }],
            usage: Default::default(),
            stop_reason,
            error_message: Some(error_message),
            provider: self.config.model.provider.clone(),
            model: self.config.model.id.clone(),
            timestamp: now_millis(),
            reasoning_details: None,
        }
    }
}

fn extract_tool_calls(assistant: &AssistantMessage) -> Vec<ToolCall> {
    assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

/// Plan 02 PR-A: return `Cow::Borrowed` on the common no-
/// sanitization path so the entire context Vec isn't cloned
/// every turn. The needs-sanitization scan inspects each
/// assistant message and short-circuits as soon as one
/// would have been filtered or rewritten; if none would,
/// the caller borrows the original slice.
fn sanitize_context_for_request<'a>(
    messages: &'a [Message],
    includes_thinking_in_replay: bool,
    requires_thinking_signature: bool,
) -> std::borrow::Cow<'a, [Message]> {
    if !context_needs_sanitization(
        messages,
        includes_thinking_in_replay,
        requires_thinking_signature,
    ) {
        return std::borrow::Cow::Borrowed(messages);
    }
    let sanitized: Vec<Message> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => sanitize_assistant_for_request(
                assistant,
                includes_thinking_in_replay,
                requires_thinking_signature,
            )
            .map(Message::Assistant),
            _ => Some(message.clone()),
        })
        .collect();
    std::borrow::Cow::Owned(sanitized)
}

/// `true` when any assistant message in `messages` would be
/// filtered or rewritten by `sanitize_assistant_for_request`.
/// Mirrors the sanitizer's own predicates — when they drift,
/// update both.
fn context_needs_sanitization(
    messages: &[Message],
    includes_thinking_in_replay: bool,
    requires_thinking_signature: bool,
) -> bool {
    messages.iter().any(|message| {
        let Message::Assistant(assistant) = message else {
            return false;
        };
        if matches!(
            assistant.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            return true;
        }
        let mut non_thinking_visible = false;
        let mut any_drop = false;
        for block in &assistant.content {
            match block {
                ContentBlock::Text { text } if text.trim().is_empty() => any_drop = true,
                ContentBlock::Text { .. } => non_thinking_visible = true,
                ContentBlock::Thinking { thinking, .. } if thinking.trim().is_empty() => {
                    any_drop = true
                }
                ContentBlock::Thinking { .. } if !includes_thinking_in_replay => any_drop = true,
                ContentBlock::Thinking {
                    signature: None, ..
                } if requires_thinking_signature => any_drop = true,
                ContentBlock::Thinking { .. } => {}
                ContentBlock::RedactedThinking { .. } if !includes_thinking_in_replay => {
                    any_drop = true
                }
                ContentBlock::RedactedThinking { .. } => {}
                _ => non_thinking_visible = true,
            }
        }
        if any_drop {
            return true;
        }
        // Sanitizer also filters out messages with no non-
        // thinking visible blocks. If the original already
        // has none, the sanitizer would filter it — sanitize
        // path needed.
        !non_thinking_visible
    })
}

fn sanitize_assistant_for_request(
    assistant: &AssistantMessage,
    includes_thinking_in_replay: bool,
    requires_thinking_signature: bool,
) -> Option<AssistantMessage> {
    if matches!(
        assistant.stop_reason,
        StopReason::Error | StopReason::Aborted
    ) {
        return None;
    }

    let content = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.trim().is_empty() => None,
            ContentBlock::Thinking { thinking, .. } if thinking.trim().is_empty() => None,
            ContentBlock::Thinking { .. } if !includes_thinking_in_replay => None,
            // Provider requires a signature on every replayed thinking
            // block (Anthropic). Unsigned blocks either came from
            // pre-capture sessions or would be rejected with 400.
            // Drop them rather than send invalid payloads.
            // See docs/api_integrity_plans/01c_serializer_and_sanitizer.md.
            ContentBlock::Thinking {
                signature: None, ..
            } if requires_thinking_signature => None,
            // Redacted thinking is the encrypted-reasoning analog of
            // a signed thinking block: only Anthropic understands it,
            // so it's dropped for providers that can't replay
            // thinking. See docs/api_integrity_plans/02.
            ContentBlock::RedactedThinking { .. } if !includes_thinking_in_replay => None,
            _ => Some(block.clone()),
        })
        .collect::<Vec<_>>();

    if content.is_empty()
        || !content.iter().any(|block| {
            !matches!(
                block,
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        })
    {
        return None;
    }

    Some(AssistantMessage {
        content,
        ..assistant.clone()
    })
}

fn validate_tool_arguments(
    validator_state: &crate::tool::ValidatorState,
    args: &serde_json::Value,
) -> Result<(), String> {
    let validator = match validator_state {
        crate::tool::ValidatorState::Ready(v) => v,
        // Preserve the legacy error wording verbatim so any
        // downstream log-match or integration test that keyed
        // on "Tool schema compilation failed:" still works.
        crate::tool::ValidatorState::Invalid(message) => return Err(message.clone()),
    };
    let errors: Vec<String> = validator
        .iter_errors(args)
        .map(|error| error.to_string())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Tool arguments failed validation: {}",
            errors.join("; ")
        ))
    }
}

fn error_tool_result(message: String) -> ToolResult {
    ToolResult {
        content: vec![ContentBlock::Text {
            text: message.clone(),
        }],
        details: serde_json::json!({ "error": message }),
    }
}

fn tool_result_message(
    tool_call: &ToolCall,
    result: ToolResult,
    is_error: bool,
) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content: result.content,
        details: result.details,
        is_error,
        timestamp: now_millis(),
    }
}

fn apply_tool_result_override(
    result: &mut ToolResult,
    is_error: &mut bool,
    override_result: ToolResultOverride,
) {
    if let Some(content) = override_result.content {
        result.content = content;
    }
    if let Some(details) = override_result.details {
        result.details = details;
    }
    if let Some(override_is_error) = override_result.is_error {
        *is_error = override_is_error;
    }
}

fn attach_tool_invocation_details(tool_call: &ToolCall, details: &mut serde_json::Value) {
    if !details.is_object() {
        *details = serde_json::json!({});
    }

    let Some(map) = details.as_object_mut() else {
        return;
    };
    map.entry("tool_name")
        .or_insert_with(|| serde_json::Value::String(tool_call.name.clone()));
    if let Some(path) = tool_call.arguments.get("path").cloned() {
        map.entry("path").or_insert(path);
    }
    if let Some(command) = tool_call.arguments.get("command").cloned() {
        map.entry("command").or_insert(command);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveDelta {
    Text,
    Thinking,
}

struct CollectedAssistant {
    assistant: AssistantMessage,
    provider_error: Option<ProviderError>,
}

struct AssistantMessageBuilder {
    content: Vec<BuilderContent>,
    provider: String,
    model: String,
}

impl AssistantMessageBuilder {
    fn new(provider: String, model: String) -> Self {
        Self {
            content: Vec::new(),
            provider,
            model,
        }
    }

    fn placeholder_message(&self) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: self.provider.clone(),
            model: self.model.clone(),
            timestamp: now_millis(),
            reasoning_details: None,
        }
    }

    fn push_text(&mut self, text: &str) {
        match self.content.last_mut() {
            Some(BuilderContent::Text(existing)) => existing.push_str(text),
            _ => self.content.push(BuilderContent::Text(text.to_string())),
        }
    }

    fn push_thinking(&mut self, thinking: &str) {
        match self.content.last_mut() {
            Some(BuilderContent::Thinking(existing)) => existing.push_str(thinking),
            _ => self
                .content
                .push(BuilderContent::Thinking(thinking.to_string())),
        }
    }

    fn start_tool_call(&mut self, tool_call: ToolCall) {
        self.content
            .push(BuilderContent::ToolCall(ToolCallBuilder::new(tool_call)));
    }

    fn append_tool_call_delta(&mut self, id: &str, arguments_delta: &str) {
        if let Some(tool_call) = self.find_tool_call_mut(id) {
            tool_call.arguments_buffer.push_str(arguments_delta);
        } else {
            warn!(
                tool_call_id = id,
                "received tool-call delta for unknown tool call"
            );
        }
    }

    fn finish_tool_call(&mut self, id: &str) {
        if let Some(tool_call) = self.find_tool_call_mut(id) {
            tool_call.finalize_arguments();
        }
    }

    fn finish(self, stop_reason: StopReason, error_message: Option<String>) -> AssistantMessage {
        let mut content: Vec<ContentBlock> = self
            .content
            .into_iter()
            .map(BuilderContent::into_content_block)
            .filter(|block| match block {
                ContentBlock::Text { text } => !text.trim().is_empty(),
                ContentBlock::Thinking { thinking, .. } => !thinking.trim().is_empty(),
                _ => true,
            })
            .collect();
        if let Some(message) = &error_message
            && content.is_empty()
        {
            content.push(ContentBlock::Text {
                text: message.clone(),
            });
        }
        AssistantMessage {
            content,
            usage: Default::default(),
            stop_reason,
            error_message,
            provider: self.provider,
            model: self.model,
            timestamp: now_millis(),
            reasoning_details: None,
        }
    }

    fn find_tool_call_mut(&mut self, id: &str) -> Option<&mut ToolCallBuilder> {
        self.content.iter_mut().find_map(|block| match block {
            BuilderContent::ToolCall(tool_call) if tool_call.id == id => Some(tool_call),
            _ => None,
        })
    }
}

enum BuilderContent {
    Text(String),
    Thinking(String),
    ToolCall(ToolCallBuilder),
}

impl BuilderContent {
    fn into_content_block(self) -> ContentBlock {
        match self {
            Self::Text(text) => ContentBlock::Text { text },
            Self::Thinking(thinking) => ContentBlock::Thinking {
                thinking,
                signature: None,
            },
            Self::ToolCall(tool_call) => ContentBlock::ToolCall(tool_call.into_tool_call()),
        }
    }
}

struct ToolCallBuilder {
    id: String,
    name: String,
    arguments_value: Option<serde_json::Value>,
    arguments_buffer: String,
}

impl ToolCallBuilder {
    fn new(tool_call: ToolCall) -> Self {
        Self {
            id: tool_call.id,
            name: tool_call.name,
            arguments_value: Some(tool_call.arguments),
            arguments_buffer: String::new(),
        }
    }

    fn finalize_arguments(&mut self) {
        if self.arguments_buffer.is_empty() {
            return;
        }

        match serde_json::from_str(&self.arguments_buffer) {
            Ok(arguments) => self.arguments_value = Some(arguments),
            Err(error) => {
                self.arguments_value = Some(serde_json::json!({
                    "_raw": self.arguments_buffer,
                    "_error": error.to_string(),
                }));
            }
        }
    }

    fn into_tool_call(mut self) -> ToolCall {
        self.finalize_arguments();
        ToolCall {
            id: self.id,
            name: self.name,
            arguments: self.arguments_value.unwrap_or(serde_json::Value::Null),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use anie_protocol::{
        AssistantMessage, ContentBlock, Message, StopReason, ToolCall, Usage, UserMessage,
    };
    use anie_provider::{
        CostPerMillion, Model, ModelCompat, ProviderError, ProviderRegistry,
        RequestOptionsResolver, ResolvedRequestOptions,
    };
    use async_trait::async_trait;
    use serde_json::json;

    use super::{
        AgentLoop, AgentLoopConfig, ToolExecutionMode, ToolRegistry,
        sanitize_assistant_for_request, sanitize_context_for_request,
    };

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

    fn sample_model() -> Model {
        Model {
            id: "mock-model".into(),
            name: "Mock Model".into(),
            provider: "mock".into(),
            api: anie_provider::ApiKind::OpenAICompletions,
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

    fn sample_agent_loop_config() -> AgentLoopConfig {
        AgentLoopConfig::new(
            sample_model(),
            "system".into(),
            anie_provider::ThinkingLevel::Off,
            ToolExecutionMode::Parallel,
            Arc::new(StaticResolver),
        )
    }

    #[test]
    fn agent_loop_config_num_ctx_override_defaults_to_none() {
        let options = sample_agent_loop_config().stream_options(None, HashMap::new());

        assert_eq!(options.num_ctx_override, None);
    }

    #[test]
    fn agent_loop_copies_num_ctx_override_into_stream_options() {
        let config = sample_agent_loop_config().with_ollama_num_ctx_override(Some(16_384));

        let options = config.stream_options(Some("key".into()), HashMap::new());

        assert_eq!(options.api_key.as_deref(), Some("key"));
        assert_eq!(options.num_ctx_override, Some(16_384));
    }

    /// Plan 05 PR A: with no override, the tool execution
    /// context inherits the catalog `context_window`. The
    /// override is the runtime knob (`/context-length`); it
    /// wins when set so tools see the same effective window
    /// as the provider.
    #[test]
    fn tool_execution_context_uses_model_context_window_without_override() {
        let config = sample_agent_loop_config();
        let agent_loop = AgentLoop::new(
            Arc::new(ProviderRegistry::default()),
            Arc::new(ToolRegistry::new()),
            config,
        );
        assert_eq!(agent_loop.tool_execution_context().context_window, 32_768);
    }

    #[test]
    fn tool_execution_context_uses_runtime_override_when_present() {
        let config = sample_agent_loop_config().with_ollama_num_ctx_override(Some(8_192));
        let agent_loop = AgentLoop::new(
            Arc::new(ProviderRegistry::default()),
            Arc::new(ToolRegistry::new()),
            config,
        );
        assert_eq!(agent_loop.tool_execution_context().context_window, 8_192);
    }

    fn user_message(text: &str, timestamp: u64) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp,
        })
    }

    fn assistant_message(
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
        timestamp: u64,
    ) -> AssistantMessage {
        AssistantMessage {
            content,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            provider: "mock".into(),
            model: "mock-model".into(),
            timestamp,
            reasoning_details: None,
        }
    }

    /// Plan 02 PR-A fast path: a context with no empty/
    /// failed/filter-eligible assistants returns
    /// `Cow::Borrowed`.
    #[test]
    fn sanitize_context_fast_path_returns_borrowed_when_no_rewrite_needed() {
        use std::borrow::Cow;

        let context = vec![
            user_message("first", 1),
            Message::Assistant(assistant_message(
                vec![ContentBlock::Text {
                    text: "visible".into(),
                }],
                StopReason::Stop,
                2,
            )),
            user_message("second", 3),
        ];

        let sanitized = sanitize_context_for_request(&context, false, false);
        assert!(
            matches!(sanitized, Cow::Borrowed(_)),
            "expected borrowed Cow on fast path"
        );
    }

    /// Plan 02 PR-A: the owned path still runs when any
    /// assistant message triggers a filter or rewrite.
    #[test]
    fn sanitize_context_owned_path_runs_when_rewrite_needed() {
        use std::borrow::Cow;

        // Empty-text assistant forces a drop — sanitizer must
        // rewrite, so the Cow is Owned.
        let context = vec![
            user_message("first", 1),
            Message::Assistant(assistant_message(
                vec![ContentBlock::Text { text: "  ".into() }],
                StopReason::Stop,
                2,
            )),
            Message::Assistant(assistant_message(
                vec![ContentBlock::Text {
                    text: "keep".into(),
                }],
                StopReason::Stop,
                3,
            )),
        ];

        let sanitized = sanitize_context_for_request(&context, false, false);
        assert!(
            matches!(sanitized, Cow::Owned(_)),
            "expected owned Cow when rewrite needed"
        );
    }

    #[test]
    fn sanitize_context_for_request_drops_empty_and_failed_assistants() {
        let context = vec![
            user_message("first", 1),
            Message::Assistant(assistant_message(Vec::new(), StopReason::Stop, 2)),
            Message::Assistant(assistant_message(
                vec![ContentBlock::Text { text: "   ".into() }],
                StopReason::Stop,
                3,
            )),
            Message::Assistant(assistant_message(
                vec![ContentBlock::Text {
                    text: "failed".into(),
                }],
                StopReason::Error,
                4,
            )),
            Message::Assistant(assistant_message(
                vec![
                    ContentBlock::Text { text: "  ".into() },
                    ContentBlock::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        arguments: json!({ "path": "README.md" }),
                    }),
                ],
                StopReason::ToolUse,
                5,
            )),
            user_message("second", 6),
        ];

        let sanitized = sanitize_context_for_request(&context, false, false);

        assert_eq!(sanitized.len(), 3);
        assert!(matches!(sanitized[0], Message::User(_)));
        let assistant = match &sanitized[1] {
            Message::Assistant(assistant) => assistant,
            other => panic!("expected assistant, got {other:?}"),
        };
        assert_eq!(assistant.content.len(), 1);
        assert!(matches!(assistant.content[0], ContentBlock::ToolCall(_)));
        assert!(matches!(sanitized[2], Message::User(_)));
    }

    #[test]
    fn sanitize_assistant_for_request_drops_thinking_only_messages() {
        let assistant = assistant_message(
            vec![
                ContentBlock::Thinking {
                    thinking: "plan first".into(),
                    signature: None,
                },
                ContentBlock::Text { text: "   ".into() },
            ],
            StopReason::Stop,
            1,
        );

        let sanitized = sanitize_assistant_for_request(&assistant, true, false);

        assert!(sanitized.is_none());
    }

    #[test]
    fn sanitize_assistant_for_request_strips_thinking_when_replay_disallowed() {
        let assistant = assistant_message(
            vec![
                ContentBlock::Thinking {
                    thinking: "plan first".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "final answer".into(),
                },
            ],
            StopReason::Stop,
            1,
        );

        let sanitized = sanitize_assistant_for_request(&assistant, false, false)
            .expect("assistant with text should survive");

        assert_eq!(
            sanitized.content,
            vec![ContentBlock::Text {
                text: "final answer".into(),
            }]
        );
    }

    #[test]
    fn sanitize_assistant_for_request_preserves_thinking_when_replay_allowed() {
        let assistant = assistant_message(
            vec![
                ContentBlock::Thinking {
                    thinking: "inspect file".into(),
                    signature: None,
                },
                ContentBlock::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({ "path": "README.md" }),
                }),
            ],
            StopReason::ToolUse,
            2,
        );

        let sanitized = sanitize_assistant_for_request(&assistant, true, false)
            .expect("tool assistant survives");

        assert_eq!(sanitized.content, assistant.content);
    }

    #[test]
    fn sanitize_drops_unsigned_thinking_when_signature_required() {
        let assistant = assistant_message(
            vec![
                ContentBlock::Thinking {
                    thinking: "unsigned reasoning".into(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            StopReason::Stop,
            1,
        );

        let sanitized = sanitize_assistant_for_request(&assistant, true, true)
            .expect("assistant with text should survive");

        assert_eq!(sanitized.content.len(), 1);
        assert!(matches!(sanitized.content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn sanitize_keeps_signed_thinking_when_signature_required() {
        let assistant = assistant_message(
            vec![
                ContentBlock::Thinking {
                    thinking: "signed reasoning".into(),
                    signature: Some("SIG_abc".into()),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            StopReason::Stop,
            1,
        );

        let sanitized = sanitize_assistant_for_request(&assistant, true, true)
            .expect("assistant with text should survive");

        assert_eq!(sanitized.content.len(), 2);
        assert!(matches!(
            &sanitized.content[0],
            ContentBlock::Thinking { signature: Some(sig), .. } if sig == "SIG_abc"
        ));
    }

    #[test]
    fn sanitize_drops_redacted_thinking_for_non_anthropic_replay() {
        let assistant = assistant_message(
            vec![
                ContentBlock::RedactedThinking {
                    data: "ENCRYPTED".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            StopReason::Stop,
            1,
        );
        // includes_thinking_in_replay=false (OpenAI-style) — the
        // redacted block has no wire representation there.
        let sanitized = sanitize_assistant_for_request(&assistant, false, false)
            .expect("assistant with text survives");
        assert_eq!(sanitized.content.len(), 1);
        assert!(matches!(sanitized.content[0], ContentBlock::Text { .. }));
    }

    #[test]
    fn sanitize_keeps_redacted_thinking_for_anthropic_replay() {
        let assistant = assistant_message(
            vec![
                ContentBlock::RedactedThinking {
                    data: "ENCRYPTED".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            StopReason::Stop,
            1,
        );
        // includes_thinking_in_replay=true, requires_sig=true (Anthropic)
        let sanitized =
            sanitize_assistant_for_request(&assistant, true, true).expect("assistant survives");
        assert_eq!(sanitized.content.len(), 2);
        assert!(matches!(
            &sanitized.content[0],
            ContentBlock::RedactedThinking { .. }
        ));
    }

    #[test]
    fn sanitize_drops_assistant_when_only_unsigned_thinking_remains() {
        let assistant = assistant_message(
            vec![ContentBlock::Thinking {
                thinking: "unsigned".into(),
                signature: None,
            }],
            StopReason::Stop,
            1,
        );

        let sanitized = sanitize_assistant_for_request(&assistant, true, true);

        assert!(sanitized.is_none());
    }
}
