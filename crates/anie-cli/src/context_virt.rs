//! Context-virtualization policy.
//!
//! Phase C of `docs/rlm_2026-04-29/06_phased_implementation.md`.
//!
//! [`ContextVirtualizationPolicy`] is a [`BeforeModelPolicy`]
//! that enforces a configurable active-context token ceiling.
//! When the run's active context exceeds the ceiling, the
//! policy evicts oldest messages — pinning the last N — until
//! the surviving subset is under ceiling, archives the full
//! snapshot to the shared [`ExternalContext`] store so the
//! recurse tool can read evicted content, and returns
//! `BeforeModelResponse::ReplaceMessages(survivors)`.
//!
//! Identity / dedup: messages are tracked by `timestamp`. The
//! agent loop generates one message per `now_millis()`
//! sample; collisions in practice are vanishingly rare. The
//! pushed-set is pre-populated at construction with the
//! run-start snapshot so we don't double-push messages that
//! were already in the store.
//!
//! Default behavior: with `active_ceiling_tokens = u64::MAX`
//! the policy is effectively a noop — it returns `Continue`
//! on every call. The controller installs the policy in
//! `--harness-mode=rlm`; default builds keep the noop policy.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse};
use anie_protocol::Message;
use anie_session::estimate_tokens;
use tokio::sync::RwLock;

use crate::external_context::ExternalContext;

/// Extract a Message's timestamp, regardless of variant.
#[allow(dead_code)] // wired up by 08.3 (controller install).
fn message_timestamp(m: &Message) -> u64 {
    match m {
        Message::User(u) => u.timestamp,
        Message::Assistant(a) => a.timestamp,
        Message::ToolResult(t) => t.timestamp,
        Message::Custom(c) => c.timestamp,
    }
}

/// Active-context ceiling + FIFO eviction policy.
///
/// Holds a shared handle to the `ExternalContext` store so
/// evicted content stays reachable via the recurse tool.
#[allow(dead_code)] // wired up by 08.3 (controller install).
pub(crate) struct ContextVirtualizationPolicy {
    /// Maximum total tokens permitted in the active context
    /// at the start of any `ModelTurn`. When the active
    /// context's token estimate exceeds this, eviction kicks
    /// in. Set to `u64::MAX` to disable the ceiling without
    /// uninstalling the policy (the `Continue` fast path
    /// short-circuits on every call).
    active_ceiling_tokens: u64,

    /// Always keep the last N messages, regardless of
    /// ceiling. Protects turn continuity — the model needs to
    /// see what just happened (current user prompt + recent
    /// tool results + recent assistant reasoning). If the
    /// pinned tail itself exceeds the ceiling, the loop is
    /// over budget but the policy stops evicting (we'd rather
    /// be over ceiling than blind to the current turn).
    keep_last_n: usize,

    /// Shared with the recurse tool's
    /// `ControllerContextProvider` so evicted messages are
    /// readable via `RecurseScope::*`. The policy writes;
    /// the recurse tool reads.
    external: Arc<RwLock<ExternalContext>>,

    /// Timestamps of messages already pushed to `external`,
    /// for dedup. Pre-populated at construction time from the
    /// run-start snapshot so we never re-push the messages
    /// that were already in the store.
    pushed: Mutex<HashSet<u64>>,
}

impl ContextVirtualizationPolicy {
    /// Build a policy bound to the given external store.
    /// Reads the store's current contents to seed the
    /// pushed-timestamps set; subsequent fires will only
    /// archive messages whose timestamps aren't already in
    /// the set.
    #[allow(dead_code)] // wired up by 08.3 (controller install).
    pub(crate) async fn new(
        active_ceiling_tokens: u64,
        keep_last_n: usize,
        external: Arc<RwLock<ExternalContext>>,
    ) -> Self {
        let pushed = {
            let store = external.read().await;
            store.iter().map(message_timestamp).collect()
        };
        Self {
            active_ceiling_tokens,
            keep_last_n,
            external,
            pushed: Mutex::new(pushed),
        }
    }
}

#[async_trait::async_trait]
impl BeforeModelPolicy for ContextVirtualizationPolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        // Sum once up front; if we're already under ceiling,
        // skip every other code path. This is the hot path
        // for runs that don't need eviction.
        let mut running_total: u64 = request
            .context
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        if running_total <= self.active_ceiling_tokens {
            return BeforeModelResponse::Continue;
        }

        // Step 1: archive any messages whose timestamps we
        // haven't seen before. The recurse tool reads from
        // `external`; if we evict without archiving, the
        // model loses the ability to recurse into that
        // content.
        //
        // We hold the std::sync::Mutex guard alongside the
        // tokio RwLock write guard; both releases happen
        // before any `.await` in this function (there are
        // none after this block), so the future stays Send.
        {
            let mut external = self.external.write().await;
            let mut pushed = self.pushed.lock().unwrap_or_else(|p| p.into_inner());
            for m in request.context.iter() {
                let ts = message_timestamp(m);
                if pushed.insert(ts) {
                    external.push(m.clone());
                }
            }
        }

        // Step 2: build the survivor list by FIFO-evicting
        // from the front of the active context until we're
        // under ceiling, never touching the pinned tail.
        // Cheap incremental subtract avoids re-summing on
        // every iteration.
        let mut survivors: Vec<Message> = request.context.to_vec();
        let mut idx = 0usize;
        while running_total > self.active_ceiling_tokens && survivors.len() - idx > self.keep_last_n
        {
            let cost = estimate_tokens(&survivors[idx]);
            running_total = running_total.saturating_sub(cost);
            idx += 1;
        }
        if idx > 0 {
            survivors.drain(..idx);
        }

        BeforeModelResponse::ReplaceMessages(survivors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{
        AssistantMessage, ContentBlock, Message, StopReason, ToolResultMessage, Usage, UserMessage,
    };
    use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat};

    fn user(text: &str, ts: u64) -> Message {
        Message::User(UserMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            timestamp: ts,
        })
    }

    fn assistant(text: &str, ts: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            provider: "test".into(),
            model: "test".into(),
            timestamp: ts,
            reasoning_details: None,
        })
    }

    fn tool_result(call_id: &str, tool_name: &str, body: &str, ts: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text { text: body.into() }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: ts,
        })
    }

    fn sample_model() -> Model {
        Model {
            id: "test".into(),
            name: "test".into(),
            provider: "test".into(),
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

    fn sample_request<'a>(context: &'a [Message]) -> BeforeModelRequest<'a> {
        BeforeModelRequest {
            context,
            generated_messages: &[],
            model: Box::leak(Box::new(sample_model())),
            step_index: 0,
        }
    }

    /// With `u64::MAX` ceiling the policy never evicts —
    /// `Continue` on every call. This is the default-install
    /// behavior the controller falls through to when the
    /// operator hasn't opted into a ceiling.
    #[tokio::test]
    async fn ceiling_unlimited_returns_continue() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let policy = ContextVirtualizationPolicy::new(u64::MAX, 4, store).await;
        let context: Vec<Message> = (0..20).map(|i| user("hello", i as u64)).collect();
        let response = policy.before_model(sample_request(&context)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
    }

    /// Under the ceiling: `Continue`. Eviction is not
    /// triggered for runs whose active context is small.
    #[tokio::test]
    async fn under_ceiling_returns_continue() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // 10-token ceiling, content is small.
        let policy = ContextVirtualizationPolicy::new(10_000, 4, store).await;
        let context = vec![user("hi", 1), assistant("hello", 2)];
        let response = policy.before_model(sample_request(&context)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
    }

    /// Over ceiling: evicts oldest first, pins the last N.
    /// With 10 messages, ceiling tight enough to require
    /// eviction, and `keep_last_n = 3`, the result keeps the
    /// last 3 at minimum and evicts older ones from the
    /// front.
    #[tokio::test]
    async fn over_ceiling_evicts_oldest_keeps_pinned_tail() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // Each user("..", ts) message is roughly 1 token of
        // text content ("msgN") plus overhead. With a tiny
        // ceiling we force eviction.
        let context: Vec<Message> = (0..10)
            .map(|i| user(&format!("msg{i}"), i as u64))
            .collect();
        // Ceiling = 5 tokens; keep_last_n = 3.
        let policy = ContextVirtualizationPolicy::new(5, 3, store).await;
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Pinned tail: at least the last 3.
        assert!(survivors.len() >= 3);
        assert!(survivors.len() < context.len());
        // The last 3 messages are the most-recent originals.
        let n = context.len();
        assert_eq!(&survivors[survivors.len() - 3..], &context[n - 3..]);
    }

    /// Pinned tail itself exceeds the ceiling: the policy
    /// keeps the tail anyway and stops evicting. We'd rather
    /// be over budget than blind to the current turn.
    #[tokio::test]
    async fn pinned_tail_overrides_ceiling() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..6).map(|i| user(&format!("msg{i}"), i as u64)).collect();
        // Ceiling = 1 token (impossibly tight); keep_last_n = 5.
        // The pinned tail (5 messages) will be over the
        // ceiling but the policy refuses to evict pinned
        // messages.
        let policy = ContextVirtualizationPolicy::new(1, 5, store).await;
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Exactly 5 survivors (the pinned tail). One was
        // evicted.
        assert_eq!(survivors.len(), 5);
        assert_eq!(&survivors[..], &context[1..]);
    }

    /// Evicted messages are archived to `external`. After
    /// eviction, every original message is reachable via the
    /// store (whether by direct lookup or scope-based
    /// search).
    #[tokio::test]
    async fn evicted_messages_archived_to_external() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..8)
            .map(|i| user(&format!("msg{i}"), 100 + i as u64))
            .collect();
        let policy = ContextVirtualizationPolicy::new(5, 2, Arc::clone(&store)).await;
        let _ = policy.before_model(sample_request(&context)).await;
        let external = store.read().await;
        // Every original message landed in external (or was
        // already there at construction). Length matches.
        assert_eq!(external.len(), 8);
    }

    /// Pre-populated external store: messages that were in
    /// external at construction are not re-pushed when seen
    /// again in active context. Dedup by timestamp.
    #[tokio::test]
    async fn pre_populated_external_does_not_double_push() {
        let context: Vec<Message> = (0..5)
            .map(|i| user(&format!("msg{i}"), 200 + i as u64))
            .collect();
        // External pre-populated with a copy of the active
        // context (this matches Phase B's
        // `from_messages(context_snapshot)`).
        let external = Arc::new(RwLock::new(ExternalContext::from_messages(context.clone())));
        let policy = ContextVirtualizationPolicy::new(5, 2, Arc::clone(&external)).await;
        let _ = policy.before_model(sample_request(&context)).await;
        let external = external.read().await;
        // Length unchanged: 5 from pre-population, 0
        // re-pushed.
        assert_eq!(external.len(), 5);
    }

    /// Tool results follow the same eviction rules — the
    /// policy doesn't special-case kinds. (Eviction is
    /// pin-by-position, not pin-by-kind, in v1.)
    #[tokio::test]
    async fn tool_results_evicted_alongside_other_messages() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context = vec![
            user("first", 1),
            tool_result("c1", "bash", "first tool output", 2),
            assistant("ack", 3),
            user("second", 4),
            tool_result("c2", "bash", "second tool output", 5),
            assistant("ack2", 6),
            user("third", 7),
        ];
        let policy = ContextVirtualizationPolicy::new(5, 2, Arc::clone(&store)).await;
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Last 2 pinned: tool_result(c2) + ack2 + third? No
        // — keep_last_n=2 keeps the last 2 only.
        assert!(survivors.len() >= 2);
        assert_eq!(&survivors[survivors.len() - 2..], &context[5..]);
        // External holds every original (archived).
        assert_eq!(store.read().await.len(), context.len());
    }
}
