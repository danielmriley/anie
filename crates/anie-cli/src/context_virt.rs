//! Context-virtualization policy.
//!
//! Phases C + D of
//! `docs/rlm_2026-04-29/06_phased_implementation.md`.
//!
//! [`ContextVirtualizationPolicy`] is a [`BeforeModelPolicy`]
//! that enforces a configurable active-context token ceiling
//! AND injects a per-turn ledger telling the model what's
//! externally available. When the run's active context
//! exceeds the ceiling, the policy evicts oldest messages —
//! pinning the last N — until the surviving subset is under
//! ceiling, archives the full snapshot to the shared
//! [`ExternalContext`] store so the recurse tool can read
//! evicted content, builds a structured ledger summarizing
//! the external state, and returns
//! `BeforeModelResponse::ReplaceMessages(survivors + ledger)`.
//!
//! The ledger is a `User` message wrapped in
//! `<system-reminder>` tags — universally compatible with
//! every provider, recognized by the model as a system note
//! rather than a user prompt. The previous turn's ledger is
//! stripped before injecting a new one (no accumulation).
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
//! on every call (no ledger, no eviction). The controller
//! installs the policy in `--harness-mode=rlm`; default
//! builds keep the noop policy. Setting
//! `ANIE_ACTIVE_CEILING_TOKENS` to a finite value turns on
//! both eviction *and* ledger injection.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse};
use anie_protocol::{ContentBlock, Message, UserMessage, now_millis};
use anie_session::estimate_tokens;
use tokio::sync::RwLock;

use crate::external_context::{ExternalContext, MessageKindLabel};

/// Extract a Message's timestamp, regardless of variant.
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

    /// Timestamp of the ledger message injected on the
    /// previous fire, if any. Used to strip the stale ledger
    /// out of `request.context` before computing the new one
    /// so successive turns don't accumulate stale ledgers.
    /// `None` until the policy injects its first ledger.
    last_ledger_ts: Mutex<Option<u64>>,
}

impl ContextVirtualizationPolicy {
    /// Build a policy bound to the given external store. The
    /// caller passes the set of timestamps already present in
    /// the store so we don't double-push them on the first
    /// fire — typically built by walking the run-start
    /// snapshot before the store is wrapped in the `RwLock`.
    pub(crate) fn new(
        active_ceiling_tokens: u64,
        keep_last_n: usize,
        external: Arc<RwLock<ExternalContext>>,
        pushed: HashSet<u64>,
    ) -> Self {
        Self {
            active_ceiling_tokens,
            keep_last_n,
            external,
            pushed: Mutex::new(pushed),
            last_ledger_ts: Mutex::new(None),
        }
    }

    /// Convenience: build the pushed-timestamps set from a
    /// `Vec<Message>` snapshot — the run-start context the
    /// controller hands to `build_rlm_extras`.
    pub(crate) fn pushed_set_from_snapshot(snapshot: &[Message]) -> HashSet<u64> {
        snapshot.iter().map(message_timestamp).collect()
    }
}

#[async_trait::async_trait]
impl BeforeModelPolicy for ContextVirtualizationPolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>) -> BeforeModelResponse {
        // Default-preserving fast path: with the ceiling at
        // u64::MAX (the noop install) we don't archive, don't
        // evict, and don't inject a ledger. Identical
        // behavior to NoopBeforeModelPolicy. Operators flip
        // this on by setting `ANIE_ACTIVE_CEILING_TOKENS`.
        if self.active_ceiling_tokens == u64::MAX {
            return BeforeModelResponse::Continue;
        }

        // Step 1: strip the previous turn's ledger out of
        // working. If we left it in, archiving would push
        // stale ledgers into `external` and the model would
        // see two ledgers (old + new) every turn.
        let stale_ledger_ts = *self
            .last_ledger_ts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut working: Vec<Message> = match stale_ledger_ts {
            None => request.context.to_vec(),
            Some(ts) => request
                .context
                .iter()
                .filter(|m| message_timestamp(m) != ts)
                .cloned()
                .collect(),
        };

        // Step 2: archive any unseen messages to `external`
        // so eviction is reversible via the recurse tool.
        // Hold both guards within one sync block — no `.await`
        // between acquire and drop — so the future stays Send.
        {
            let mut external = self.external.write().await;
            let mut pushed = self.pushed.lock().unwrap_or_else(|p| p.into_inner());
            for m in &working {
                let ts = message_timestamp(m);
                if pushed.insert(ts) {
                    external.push(m.clone());
                }
            }
        }

        // Step 3: FIFO-evict from the front while over
        // ceiling and outside the pinned tail.
        let mut running_total: u64 = working
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        let mut idx = 0usize;
        while running_total > self.active_ceiling_tokens
            && working.len().saturating_sub(idx) > self.keep_last_n
        {
            let cost = estimate_tokens(&working[idx]);
            running_total = running_total.saturating_sub(cost);
            idx += 1;
        }
        if idx > 0 {
            working.drain(..idx);
        }

        // Step 4: build the ledger from current external
        // state and append it. The ledger sits at the very
        // end of working, right before the model generates,
        // so it's maximally visible to the model.
        let ledger = self.build_ledger(working.len()).await;
        let ledger_ts = message_timestamp(&ledger);
        working.push(ledger);

        // Record the ledger timestamp so the next fire
        // strips it. Also add to `pushed` so we don't
        // archive it (the ledger isn't real conversational
        // content; the recurse tool shouldn't surface it).
        *self
            .last_ledger_ts
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(ledger_ts);
        self.pushed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(ledger_ts);

        BeforeModelResponse::ReplaceMessages(working)
    }
}

impl ContextVirtualizationPolicy {
    /// Build the structured ledger as a `User` message
    /// wrapped in `<system-reminder>` tags. Counts come from
    /// the shared `ExternalContext` indexes; tool-result
    /// breakdown is sorted by frequency and capped at 8 names
    /// to keep the ledger bounded (target ≤500 tokens).
    async fn build_ledger(&self, active_len: usize) -> Message {
        let lines = {
            let external = self.external.read().await;
            let total = external.len();
            let evicted = total.saturating_sub(active_len);

            let mut lines = vec![
                "<system-reminder>".to_string(),
                "external context — call the recurse tool to access evicted content".to_string(),
                format!("- {total} total messages ({evicted} evicted, {active_len} active)"),
            ];

            // Tool-result breakdown by tool name. Walk the
            // ToolResult ID list once, count per tool name.
            let tool_result_ids = external.ids_by_kind(MessageKindLabel::ToolResult);
            if !tool_result_ids.is_empty() {
                let mut counts: HashMap<String, usize> = HashMap::new();
                for &id in tool_result_ids {
                    if let Some(Message::ToolResult(t)) = external.get_by_id(id) {
                        *counts.entry(t.tool_name.clone()).or_default() += 1;
                    }
                }
                let mut sorted: Vec<(String, usize)> = counts.into_iter().collect();
                sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                let parts = sorted
                    .iter()
                    .take(8)
                    .map(|(n, c)| format!("{n} x{c}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                lines.push(format!(
                    "- {} tool results: {}",
                    tool_result_ids.len(),
                    parts
                ));
            }

            lines.push("</system-reminder>".to_string());
            lines
        };

        Message::User(UserMessage {
            content: vec![ContentBlock::Text {
                text: lines.join("\n"),
            }],
            timestamp: now_millis(),
        })
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
        let policy = ContextVirtualizationPolicy::new(u64::MAX, 4, store, HashSet::new());
        let context: Vec<Message> = (0..20).map(|i| user("hello", i as u64)).collect();
        let response = policy.before_model(sample_request(&context)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
    }

    /// Finite ceiling, under-ceiling content: no eviction
    /// happens but the policy still injects a ledger so the
    /// model knows the recurse tool is available. The
    /// originals come through unchanged; the ledger is
    /// appended at the very end.
    #[tokio::test]
    async fn under_ceiling_keeps_all_messages_and_appends_ledger() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // 10_000-token ceiling, content is tiny.
        let policy = ContextVirtualizationPolicy::new(10_000, 4, store, HashSet::new());
        let context = vec![user("hi", 1), assistant("hello", 2)];
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // 2 originals + 1 ledger.
        assert_eq!(survivors.len(), 3);
        assert_eq!(&survivors[..2], &context[..]);
        assert!(is_ledger(&survivors[2]));
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
        let policy = ContextVirtualizationPolicy::new(5, 3, store, HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Pinned tail (last 3 originals) + ledger at the
        // very end.
        assert!(is_ledger(survivors.last().expect("non-empty")));
        let n = context.len();
        let originals = &survivors[..survivors.len() - 1];
        assert!(originals.len() >= 3);
        assert!(originals.len() < context.len());
        assert_eq!(&originals[originals.len() - 3..], &context[n - 3..]);
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
        let policy = ContextVirtualizationPolicy::new(1, 5, store, HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Exactly 5 originals (the pinned tail) + 1 ledger.
        // One original was evicted from the front.
        assert_eq!(survivors.len(), 6);
        assert!(is_ledger(survivors.last().expect("non-empty")));
        assert_eq!(&survivors[..5], &context[1..]);
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
        let policy = ContextVirtualizationPolicy::new(5, 2, Arc::clone(&store), HashSet::new());
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
        // Pre-populated dedup set matching the snapshot
        // currently in the store.
        let pushed = ContextVirtualizationPolicy::pushed_set_from_snapshot(&context);
        let policy = ContextVirtualizationPolicy::new(5, 2, Arc::clone(&external), pushed);
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
        let policy = ContextVirtualizationPolicy::new(5, 2, Arc::clone(&store), HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Pinned tail (last 2 originals) + ledger at the
        // end. keep_last_n=2 keeps assistant("ack2") and
        // user("third").
        assert!(is_ledger(survivors.last().expect("non-empty")));
        let originals = &survivors[..survivors.len() - 1];
        assert!(originals.len() >= 2);
        assert_eq!(&originals[originals.len() - 2..], &context[5..]);
        // External holds every original (archived); the
        // ledger itself is *not* archived, so length matches
        // the input context.
        assert_eq!(store.read().await.len(), context.len());
    }

    /// Ledger does not accumulate across fires: each turn
    /// strips the previous turn's ledger before injecting a
    /// fresh one. After two fires the survivors should
    /// contain exactly one ledger, not two.
    #[tokio::test]
    async fn ledger_replaced_each_turn_no_accumulation() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..3).map(|i| user(&format!("msg{i}"), i as u64)).collect();
        let policy = ContextVirtualizationPolicy::new(10_000, 8, store, HashSet::new());

        // Fire 1: ledger appended.
        let r1 = policy.before_model(sample_request(&context)).await;
        let after_fire_1 = match r1 {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        assert_eq!(after_fire_1.len(), 4);
        assert_eq!(after_fire_1.iter().filter(|m| is_ledger(m)).count(), 1);

        // Fire 2: feed the post-fire-1 context back in (this
        // is what the loop does — it persists the
        // ReplaceMessages output as the new state).
        let r2 = policy.before_model(sample_request(&after_fire_1)).await;
        let after_fire_2 = match r2 {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Still exactly one ledger; old one was stripped.
        assert_eq!(after_fire_2.len(), 4);
        assert_eq!(after_fire_2.iter().filter(|m| is_ledger(m)).count(), 1);
    }

    /// Ledger reflects current external state: when external
    /// holds N messages of various kinds, the ledger text
    /// names the count and the tool-result breakdown.
    #[tokio::test]
    async fn ledger_reflects_external_state() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context = vec![
            user("u0", 1),
            tool_result("c1", "bash", "ls", 2),
            tool_result("c2", "bash", "pwd", 3),
            tool_result("c3", "read", "file", 4),
            assistant("ack", 5),
        ];
        let policy = ContextVirtualizationPolicy::new(10_000, 8, store, HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ledger_text = match survivors.last().expect("non-empty") {
            Message::User(u) => match &u.content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text"),
            },
            _ => panic!("expected User ledger"),
        };
        assert!(ledger_text.contains("<system-reminder>"));
        assert!(ledger_text.contains("recurse tool"));
        assert!(ledger_text.contains("5 total messages"));
        assert!(ledger_text.contains("3 tool results"));
        assert!(ledger_text.contains("bash x2"));
        assert!(ledger_text.contains("read x1"));
    }

    /// Ledger is not archived to `external` — the recurse
    /// tool surfaces conversational content, not policy
    /// metadata. After a fire that injects a ledger, the
    /// store size matches the input count exactly.
    #[tokio::test]
    async fn ledger_not_archived_to_external() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..3).map(|i| user(&format!("msg{i}"), i as u64)).collect();
        let policy =
            ContextVirtualizationPolicy::new(10_000, 8, Arc::clone(&store), HashSet::new());
        let _ = policy.before_model(sample_request(&context)).await;
        // External: 3 originals, 0 ledgers.
        assert_eq!(store.read().await.len(), 3);
    }

    /// Helper: identifies our ledger messages. The wire
    /// shape is `User` with a `<system-reminder>` opening
    /// tag in the first text block; tests use this to find
    /// the ledger inside survivors.
    fn is_ledger(m: &Message) -> bool {
        match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text.starts_with("<system-reminder>"),
                _ => false,
            },
            _ => false,
        }
    }
}
