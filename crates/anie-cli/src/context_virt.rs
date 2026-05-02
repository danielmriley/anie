//! Context-virtualization policy.
//!
//! Phases C + D + E + F of
//! `docs/rlm_2026-04-29/06_phased_implementation.md`.
//!
//! [`ContextVirtualizationPolicy`] is a [`BeforeModelPolicy`]
//! that enforces a configurable active-context token ceiling,
//! pages relevant evicted content back in for the current
//! turn, and injects a per-turn ledger telling the model
//! what's externally available. When the run's active context
//! exceeds the ceiling, the policy: (1) evicts oldest
//! messages pinning the last N; (2) archives every snapshot
//! message to the shared [`ExternalContext`] so the recurse
//! tool can still reach evicted content; (3) scores evicted
//! content against the current user prompt via keyword
//! overlap and pages back in the highest-scoring messages
//! within `relevance_budget_tokens`; (4) builds a structured
//! ledger; (5) returns
//! `BeforeModelResponse::ReplaceMessages(working + ledger)`.
//!
//! Pinning rules (eviction-resistant):
//! - The latest `User` message is always preserved
//!   (rlm/17). Without this, tight ceilings can evict the
//!   user's directive itself, leading the model to
//!   confabulate a task from contextual cues.
//! - The last `keep_last_n` messages by position are
//!   preserved. Protects turn continuity (current prompt
//!   + recent assistant/tool work).
//!
//! These two pins compose: a pinned user message can be at
//! any position; the pinned tail is always the trailing
//! window. When both pins together exceed the ceiling, the
//! policy stops evicting and accepts being over budget —
//! correctness over budget compliance.
//!
//! The ledger is a `User` message wrapped in
//! `<system-reminder>` tags — universally compatible with
//! every provider, recognized by the model as a system note
//! rather than a user prompt. The previous turn's ledger is
//! stripped before injecting a new one (no accumulation).
//!
//! Relevance reranker: keyword overlap. The current user
//! prompt is tokenized (lowercase, alphanumeric split,
//! 3-char minimum, common stopwords filtered); each evicted
//! message is tokenized the same way; score is the size of
//! the token-set intersection. Tie-break by recency. Cheap
//! enough to run on every fire. When a candidate's full
//! body wouldn't fit under the budget, the reranker falls
//! back to the Phase-F summary (if one's been written)
//! before skipping — fitting more relevant matches into
//! the same budget at lossy fidelity.
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
//! on every call (no ledger, no eviction, no paging). The
//! controller installs the policy in `--harness-mode=rlm`;
//! default builds keep the noop policy. Setting
//! `ANIE_ACTIVE_CEILING_TOKENS` to a finite value turns on
//! the full eviction + ledger + relevance pipeline.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse, stable_args_hash};
use anie_protocol::{AgentEvent, ContentBlock, Message, UserMessage, now_millis};
use anie_session::estimate_tokens;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info};

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

/// Common English stopwords we drop during tokenization so
/// they don't dominate keyword overlap. Small fixed set; the
/// reranker's job is to find topical matches, not common
/// connective tissue. Kept short on purpose — every word
/// here is one that appears in nearly every message.
const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "this",
    "that",
    "from",
    "are",
    "was",
    "were",
    "have",
    "has",
    "had",
    "but",
    "not",
    "you",
    "your",
    "all",
    "any",
    "can",
    "will",
    "would",
    "should",
    "could",
    "what",
    "when",
    "where",
    "why",
    "how",
    "which",
    "who",
    "into",
    "out",
    "over",
    "under",
    "between",
    "through",
    "during",
    "before",
    "after",
    "again",
    "then",
    "once",
    "here",
    "there",
    "more",
    "most",
    "some",
    "such",
    "only",
    "own",
    "same",
    "than",
    "too",
    "very",
    "just",
    "now",
    "also",
    "about",
    "they",
    "them",
    "their",
    "its",
    "itself",
    "been",
    "being",
    "ourselves",
];

/// Tokenize text for keyword-overlap scoring: lowercase,
/// split on non-alphanumeric, drop short tokens, drop
/// stopwords. Returns a `HashSet` so intersection size
/// is the score.
fn tokenize(s: &str) -> HashSet<String> {
    s.to_ascii_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3 && !STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

/// First text block in a content vector, if any. The
/// reranker only scores the textual portion; tool inputs /
/// outputs / images get summarized to "" (zero score) for
/// v1.
fn first_text(blocks: &[ContentBlock]) -> Option<&str> {
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            return Some(text.as_str());
        }
    }
    None
}

/// Convenience for whole messages: returns the first
/// text block of the message's content. Used by the
/// archive-time embed enqueue path. Custom messages
/// have no surface text (their payload is opaque
/// JSON).
fn first_text_of(m: &Message) -> Option<&str> {
    match m {
        Message::User(u) => first_text(&u.content),
        Message::Assistant(a) => first_text(&a.content),
        Message::ToolResult(t) => first_text(&t.content),
        Message::Custom(_) => None,
    }
}

/// Tokenize the most recent `User` message in `working` —
/// our proxy for "the model's current request." Returns
/// `None` when no user message has any text content (rare;
/// e.g., images-only) so the reranker can short-circuit.
fn current_prompt_tokens(working: &[Message]) -> Option<HashSet<String>> {
    for m in working.iter().rev() {
        if let Message::User(u) = m {
            if let Some(text) = first_text(&u.content) {
                let toks = tokenize(text);
                if !toks.is_empty() {
                    return Some(toks);
                }
            }
        }
    }
    None
}

/// One candidate the relevance reranker is considering for
/// paging back in. Carries the score (cosine similarity
/// when embeddings are available, keyword-overlap intersection
/// size cast to f32 otherwise), the archive entry's stable id
/// (for the summary-fallback annotation), the full message
/// body, and the optional pre-computed summary.
struct RelevanceCandidate {
    /// Cosine similarity in [-1, 1] when scored by
    /// embedding; intersection-size as f32 when scored by
    /// keyword overlap. Higher is better for both — we
    /// can sort uniformly without normalizing because the
    /// fallback only fires per-candidate (we never mix
    /// scores in the same sort).
    score: f32,
    id: crate::external_context::MessageId,
    message: Message,
    summary: Option<String>,
}

/// Score a candidate using the best available signal. If
/// both the prompt and the candidate have embeddings,
/// returns cosine similarity (range [-1, 1]). Otherwise
/// falls back to keyword overlap (intersection size cast
/// to f32, range [0, ∞)). The two scales aren't
/// commensurate but they only ever appear in the same
/// sort when the run is mid-warmup (some candidates
/// embedded, some not) — both are "higher is better" so
/// the relative ordering remains useful in either
/// regime.
fn score_candidate(
    prompt_embed: Option<&[f32]>,
    prompt_tokens: &HashSet<String>,
    candidate_embed: Option<&[f32]>,
    candidate_message: &Message,
) -> f32 {
    if let (Some(p), Some(c)) = (prompt_embed, candidate_embed) {
        return crate::embedder::cosine_similarity(p, c);
    }
    score_message(prompt_tokens, candidate_message) as f32
}

/// Score a candidate message against tokenized prompt
/// keywords. The score is the size of the intersection of
/// the prompt's tokens and the message's tokens. Tool
/// results, assistants, and users all contribute their
/// first text block; custom messages get score 0.
fn score_message(prompt_tokens: &HashSet<String>, m: &Message) -> usize {
    let text = match m {
        Message::User(u) => first_text(&u.content),
        Message::Assistant(a) => first_text(&a.content),
        Message::ToolResult(t) => first_text(&t.content),
        Message::Custom(_) => None,
    };
    let Some(text) = text else { return 0 };
    let msg_tokens = tokenize(text);
    prompt_tokens.intersection(&msg_tokens).count()
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

    /// Token budget for relevance-based paging-in (Phase E).
    /// Sits *on top* of `active_ceiling_tokens`: after
    /// FIFO eviction lands `working` at ≤ ceiling, the
    /// reranker may add up to this many tokens of
    /// keyword-relevant evicted content. Set to 0 to disable
    /// paging entirely (FIFO-only behavior, equivalent to
    /// pure Phase C).
    relevance_budget_tokens: u64,

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

    /// Optional sender for per-fire breadcrumbs to the user.
    /// Set by the controller when building the policy so
    /// eviction / paging events surface as `SystemMessage`s
    /// in the transcript. `None` in tests where we exercise
    /// the policy directly.
    event_tx: Option<mpsc::Sender<AgentEvent>>,

    /// Externally-readable snapshot of `external.len()`
    /// after the most recent fire. The status bar reads
    /// this without taking the `RwLock`, so the user can
    /// see the archive growing in rlm mode without paying
    /// for synchronization on every render.
    external_size: Arc<AtomicUsize>,

    /// Optional handle to the Phase-F background
    /// summarizer worker. When `Some`, the policy enqueues
    /// summarize requests for newly-archived messages above
    /// the size threshold. `None` in tests + when the
    /// summarizer is disabled.
    summarizer_tx: Option<mpsc::Sender<crate::bg_summarizer::SummaryRequest>>,

    /// Optional Plan-08 embedder used to embed the
    /// prompt at fire time. When set, the reranker scores
    /// candidates with cached embeddings via cosine
    /// similarity instead of keyword overlap. `None`
    /// preserves the existing keyword-only behavior.
    embedder: Option<Arc<dyn crate::embedder::Embedder>>,

    /// Optional Plan-08 background embed worker handle.
    /// When set, the policy enqueues an `EmbedRequest`
    /// for each newly-archived message above the size
    /// threshold, mirroring the summarizer flow. The
    /// worker writes embeddings back into the store; the
    /// reranker reads them next turn.
    embed_tx: Option<mpsc::Sender<crate::bg_embedder::EmbedRequest>>,

    /// Per-fire cache for the prompt embedding. Keyed by
    /// the latest User message's timestamp; reused when
    /// the same prompt drives multiple fires within a
    /// turn (which happens whenever the loop spins more
    /// than one ModelTurn step). Avoids re-embedding the
    /// same prompt on every fire.
    cached_prompt_embed: tokio::sync::Mutex<Option<(u64, Vec<f32>)>>,
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
        relevance_budget_tokens: u64,
        external: Arc<RwLock<ExternalContext>>,
        pushed: HashSet<u64>,
    ) -> Self {
        Self {
            active_ceiling_tokens,
            keep_last_n,
            relevance_budget_tokens,
            external,
            pushed: Mutex::new(pushed),
            last_ledger_ts: Mutex::new(None),
            event_tx: None,
            external_size: Arc::new(AtomicUsize::new(0)),
            summarizer_tx: None,
            embedder: None,
            embed_tx: None,
            cached_prompt_embed: tokio::sync::Mutex::new(None),
        }
    }

    /// Attach a Plan-08 embedder + background worker. The
    /// reranker will score candidates with cosine
    /// similarity against the prompt's embedding when
    /// both the prompt and a candidate have embeddings;
    /// otherwise it falls back to keyword overlap for
    /// that candidate. The worker tx is held separately
    /// so the policy can enqueue archive entries for
    /// async embedding.
    pub(crate) fn with_embedder(
        mut self,
        embedder: Arc<dyn crate::embedder::Embedder>,
        tx: mpsc::Sender<crate::bg_embedder::EmbedRequest>,
    ) -> Self {
        self.embedder = Some(embedder);
        self.embed_tx = Some(tx);
        self
    }

    /// Attach a Phase-F background summarizer queue. The
    /// policy will enqueue summarize requests for newly-
    /// archived messages over the
    /// [`SUMMARIZE_MIN_TOKENS`] threshold. The worker
    /// itself is owned by the controller; this just gives
    /// the policy a handle to dispatch work to it.
    pub(crate) fn with_summarizer(
        mut self,
        tx: mpsc::Sender<crate::bg_summarizer::SummaryRequest>,
    ) -> Self {
        self.summarizer_tx = Some(tx);
        self
    }

    /// Attach an event sender so the policy can emit user-
    /// visible breadcrumbs (`SystemMessage`s) when eviction
    /// or paging fires. The controller calls this after
    /// constructing the policy.
    pub(crate) fn with_event_sender(mut self, tx: mpsc::Sender<AgentEvent>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Replace the policy's internal external-size atomic
    /// with a controller-owned one, so the status bar can
    /// observe `external.len()` across runs without
    /// re-plumbing per-run handles. The atomic is updated
    /// after every successful fire.
    pub(crate) fn with_external_size_atomic(mut self, atomic: Arc<AtomicUsize>) -> Self {
        self.external_size = atomic;
        self
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
            debug!(
                target: "anie_cli::context_virt",
                "rlm policy fire skipped (ceiling=u64::MAX, noop fast path)"
            );
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
        // Capture the post-archive size + the (id, message)
        // pairs we just inserted so we can hand them off to
        // the Phase-F summarizer below.
        let (archived_total, newly_archived): (
            usize,
            Vec<(crate::external_context::MessageId, Message)>,
        ) = {
            let mut external = self.external.write().await;
            let mut pushed = self.pushed.lock().unwrap_or_else(|p| p.into_inner());
            let mut newly_archived = Vec::new();
            for m in &working {
                let ts = message_timestamp(m);
                if pushed.insert(ts) {
                    let id = external.push(m.clone());
                    newly_archived.push((id, m.clone()));
                }
            }
            (external.len(), newly_archived)
        };
        self.external_size.store(archived_total, Ordering::Release);

        // Step 2.5 (Phase F + Plan 08): fan newly-archived
        // messages out to background workers if any are
        // wired up. Summarizer + embedder share the same
        // archive but are independent workers; we walk the
        // newly-archived list once and dispatch to each
        // worker that's configured. Skip messages below the
        // size threshold — they don't benefit from
        // summarization or embedding (keyword overlap is
        // adequate for short texts).
        let summarize_min = crate::bg_summarizer::SUMMARIZE_MIN_TOKENS;
        let embed_min = crate::bg_embedder::EMBED_MIN_TOKENS;
        for (id, message) in &newly_archived {
            let cost = estimate_tokens(message);
            if let Some(tx) = &self.summarizer_tx {
                if cost >= summarize_min {
                    let _ = tx.try_send(crate::bg_summarizer::SummaryRequest {
                        id: *id,
                        message: message.clone(),
                    });
                }
            }
            if let Some(tx) = &self.embed_tx {
                if cost >= embed_min {
                    if let Some(text) = first_text_of(message) {
                        let _ = tx.try_send(crate::bg_embedder::EmbedRequest {
                            id: *id,
                            text: text.to_string(),
                        });
                    }
                }
            }
        }
        // Drop newly_archived; we no longer need to hold
        // the clones.
        drop(newly_archived);

        // Capture the latest User message's timestamp before
        // eviction. The user's current directive must always
        // stay in active context — without it, the model
        // loses anchor and hallucinates a task from
        // contextual cues. (Observed in smoke testing:
        // qwen3.5:9b under a 1.5k ceiling + KEEP_LAST_N=2
        // confabulated a fix narrative for `SummaryOutputStore`
        // — a struct that doesn't exist — because the
        // user's "just say done" directive had been evicted.)
        let pinned_user_ts: Option<u64> = working.iter().rev().find_map(|m| match m {
            Message::User(u) => Some(u.timestamp),
            _ => None,
        });

        // Step 3: evict to bring total under ceiling.
        //
        // Priority order (PR 4 of `docs/harness_mitigations_2026-05-01/`):
        //   3a. **Supersedable failures first.** A failed
        //       tool result whose `(tool_name, args_hash)`
        //       matches a later successful call is just
        //       noise — the failure adds no information the
        //       success doesn't already convey. Cursor's
        //       harness post-mortem calls this "context
        //       rot": failed tool results that linger past
        //       their relevance degrade later decisions.
        //   3b. **Standard FIFO** for the remainder.
        //
        // The pinned user message + pinned tail are still
        // skipped (correctness guarantees from rlm/17).
        let mut running_total: u64 = working
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        let working_len = working.len();
        let pinned_tail_start = working_len.saturating_sub(self.keep_last_n);
        let mut to_evict: Vec<usize> = Vec::new();
        let mut supersedable_evicted: usize = 0;

        // 3a. Supersedable failures.
        let supersedable = find_supersedable_failures(&working);
        for &idx in &supersedable {
            if running_total <= self.active_ceiling_tokens {
                break;
            }
            // Don't touch the pinned tail.
            if idx >= pinned_tail_start {
                continue;
            }
            // Pinned user can't be a tool result anyway, but
            // be defensive.
            let is_pinned_user = matches!(working.get(idx), Some(Message::User(u))
                if pinned_user_ts == Some(u.timestamp));
            if is_pinned_user {
                continue;
            }
            if let Some(m) = working.get(idx) {
                running_total = running_total.saturating_sub(estimate_tokens(m));
                to_evict.push(idx);
                supersedable_evicted += 1;
            }
        }

        // 3b. Standard FIFO eviction for the remainder.
        for (idx, m) in working.iter().enumerate() {
            if running_total <= self.active_ceiling_tokens {
                break;
            }
            // Skip pinned tail.
            if idx >= pinned_tail_start {
                break;
            }
            // Already evicted by 3a.
            if to_evict.contains(&idx) {
                continue;
            }
            // Skip pinned user message regardless of position.
            let is_pinned_user = matches!(m, Message::User(u)
                if pinned_user_ts == Some(u.timestamp));
            if is_pinned_user {
                continue;
            }
            running_total = running_total.saturating_sub(estimate_tokens(m));
            to_evict.push(idx);
        }
        // After 3a + 3b, indices may not be sorted; sort
        // ascending so the reverse-drop below removes the
        // right items.
        to_evict.sort();
        to_evict.dedup();
        if supersedable_evicted > 0 {
            debug!(
                supersedable_evicted,
                fifo_evicted = to_evict.len() - supersedable_evicted,
                "rlm policy evicted supersedable failures + FIFO"
            );
        }
        let evicted_count = to_evict.len();
        if !to_evict.is_empty() {
            // Drop in reverse so earlier indices stay valid.
            for &idx in to_evict.iter().rev() {
                working.remove(idx);
            }
        }

        // Step 3.5: relevance-based paging-in. Score every
        // evicted message against the current prompt's
        // keywords; page back the highest scorers within
        // `relevance_budget_tokens`. This budget overlays on
        // top of the active ceiling so total send is at
        // most `active_ceiling + relevance_budget`. After
        // adding, sort working by timestamp so paged-in
        // messages land at their original chronological
        // position.
        let paged_in_count = self.page_in_relevant(&mut working).await;

        // Step 4: build the ledger from current external
        // state and append it. The ledger sits at the very
        // end of working, right before the model generates,
        // so it's maximally visible to the model.
        let ledger = self.build_ledger(working.len(), paged_in_count).await;
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

        // Tracing: a single info-level line per fire so
        // operators tailing the log can reconstruct what
        // the policy did each turn without sifting through
        // the TUI's transcript. Goes alongside the
        // `RlmStatsUpdate` event below — TUI and log are
        // independent surfaces for the same data.
        let active_tokens: u64 = working
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        info!(
            target: "anie_cli::context_virt",
            archived_total,
            evicted = evicted_count,
            paged_in = paged_in_count,
            active_tokens,
            ceiling = self.active_ceiling_tokens,
            keep_last_n = self.keep_last_n,
            "rlm policy fire"
        );

        if let Some(tx) = &self.event_tx {
            // Always emit a stats update so the status
            // bar's `archive: N msgs` field tracks even
            // turns where eviction didn't fire — the
            // archive grows by 1+ messages every turn just
            // from new assistant/tool content getting
            // pushed into it.
            let _ = tx
                .send(AgentEvent::RlmStatsUpdate {
                    archived_messages: archived_total as u64,
                })
                .await;
            // Breadcrumb: only on meaningful work
            // (evicted_count > 0 OR paged_in_count > 0). No-op
            // fires (under-ceiling, no candidates) would
            // otherwise flood the transcript.
            if evicted_count > 0 || paged_in_count > 0 {
                let _ = tx
                    .send(AgentEvent::SystemMessage {
                        text: format_breadcrumb(evicted_count, paged_in_count, archived_total),
                    })
                    .await;
            }
        }

        BeforeModelResponse::ReplaceMessages(working)
    }
}

/// Maps known tool names to (label_for_ledger,
/// arg_field_name). Tools not listed here get a generic
/// "args" label and the entire arguments JSON is shown.
/// The label is the plural noun the ledger uses to
/// describe the values (`web_read targets: a, b, c`).
const TOOL_CALL_KEYS: &[(&str, &str, &str)] = &[
    ("web_read", "targets", "url"),
    ("web_search", "queries", "query"),
    ("bash", "commands", "command"),
    ("read", "paths", "path"),
    ("edit", "paths", "path"),
    ("write", "paths", "path"),
];

/// Maximum displayed identity entries per tool. The ledger
/// has a soft 500-token target; even at 8 URL strings × ~80
/// chars × 6 tool kinds we stay well under.
const TOOL_CALL_DISPLAY_CAP: usize = 8;

/// Maximum length of a single ledger entry (URL, query,
/// command). Anything longer gets truncated with an
/// ellipsis. Keeps very long URLs from blowing past the
/// ledger budget.
const TOOL_CALL_ENTRY_MAX_CHARS: usize = 80;

/// Walk the external archive's Assistant messages and
/// One tool-call entry: the tool's `tool_call_id` (real
/// runtime id, e.g. `ollama_tool_call_8_2`) plus its
/// representative argument value (URL, query, command,
/// path). Surfacing the id alongside the arg is what makes
/// `RecurseScope::ToolResult { tool_call_id }` actually
/// usable — without the id, the model has to guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCallEntry {
    pub tool_call_id: String,
    pub arg_value: String,
}

/// Walk the external archive's Assistant messages and
/// return a per-tool list of unique tool-call entries
/// (id + meaningful arg). Used by `build_ledger` to
/// surface tool-call identity to the model.
fn collect_tool_call_summary(external: &ExternalContext) -> Vec<(String, Vec<ToolCallEntry>)> {
    let assistant_ids = external.ids_by_kind(MessageKindLabel::Assistant);
    let mut by_tool: HashMap<String, Vec<ToolCallEntry>> = HashMap::new();
    // Dedup by (tool_name, arg_value) so the same URL
    // fetched twice doesn't appear twice. The retained
    // entry keeps the *first* tool_call_id seen — that's
    // the canonical reference for that arg.
    let mut seen: HashMap<String, HashSet<String>> = HashMap::new();
    for &id in assistant_ids {
        let Some(Message::Assistant(a)) = external.get_by_id(id) else {
            continue;
        };
        for block in &a.content {
            let ContentBlock::ToolCall(call) = block else {
                continue;
            };
            let arg_value = tool_call_arg_value(&call.name, &call.arguments);
            let Some(arg_value) = arg_value else {
                continue;
            };
            let truncated = truncate_for_ledger(&arg_value, TOOL_CALL_ENTRY_MAX_CHARS);
            let dedupe = seen.entry(call.name.clone()).or_default();
            if dedupe.insert(truncated.clone()) {
                by_tool
                    .entry(call.name.clone())
                    .or_default()
                    .push(ToolCallEntry {
                        tool_call_id: call.id.clone(),
                        arg_value: truncated,
                    });
            }
        }
    }
    let mut out: Vec<(String, Vec<ToolCallEntry>)> = by_tool.into_iter().collect();
    // Stable display order: alphabetical by tool name.
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Extract the meaningful single-string arg from a tool
/// call's JSON arguments based on the tool name. Returns
/// `None` for tools we don't recognize or for malformed
/// arguments.
fn tool_call_arg_value(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    let field = TOOL_CALL_KEYS
        .iter()
        .find(|(name, _, _)| *name == tool_name)
        .map(|(_, _, field)| *field)?;
    arguments
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Truncate a string to `max_chars` characters (Unicode
/// code points), appending "…" if truncated.
fn truncate_for_ledger(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut buf: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    buf.push('…');
    buf
}

/// Render `(tool_name, entries)` pairs into ledger lines
/// like `- web_read targets: [id=ollama_tc_8_2] foo,
/// [id=ollama_tc_8_3] bar`. Each entry surfaces the
/// `tool_call_id` so `RecurseScope::ToolResult` is
/// directly usable. Empty input yields no lines so the
/// ledger stays compact when nothing has been called yet.
fn render_tool_call_summary_lines(summary: &[(String, Vec<ToolCallEntry>)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (tool_name, args) in summary {
        if args.is_empty() {
            continue;
        }
        let label = TOOL_CALL_KEYS
            .iter()
            .find(|(name, _, _)| *name == tool_name.as_str())
            .map(|(_, label, _)| *label)
            .unwrap_or("args");
        let total = args.len();
        let display = total.min(TOOL_CALL_DISPLAY_CAP);
        // Format: `<value> (id=<tool_call_id>)`. Value
        // first because that's what the model needs to match
        // against the user's question. The `(id=...)` suffix
        // is the recurse-tool reference. Earlier `[id=X] Y`
        // was misread as "the value is `[id=X]`" — qwen3.5:9b
        // was passing the bracketed string as a tool_call_id.
        let rendered: Vec<String> = args
            .iter()
            .take(display)
            .map(|e| format!("{} (id={})", e.arg_value, e.tool_call_id))
            .collect();
        let suffix = if total > display {
            format!(", +{} more", total - display)
        } else {
            String::new()
        };
        lines.push(format!(
            "- {tool_name} {label}: {}{suffix}",
            rendered.join(", ")
        ));
    }
    lines
}

/// Render the per-fire breadcrumb shown in the transcript
/// when the rlm policy does meaningful work. Compact,
/// single-line; the user reads this to confirm "yes, the
/// virtualization is doing something."
fn format_breadcrumb(evicted: usize, paged_in: usize, archived_total: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if evicted > 0 {
        parts.push(format!(
            "evicted {evicted} msg{} to external store",
            if evicted == 1 { "" } else { "s" }
        ));
    }
    if paged_in > 0 {
        parts.push(format!(
            "paged in {paged_in} relevant msg{}",
            if paged_in == 1 { "" } else { "s" }
        ));
    }
    format!(
        "rlm: {} (archive: {archived_total} msg{})",
        parts.join("; "),
        if archived_total == 1 { "" } else { "s" }
    )
}

impl ContextVirtualizationPolicy {
    /// Score evicted messages against the current prompt's
    /// keywords and append the highest-scoring ones to
    /// `working`, up to `relevance_budget_tokens`. Returns
    /// the number of messages paged in (used by the ledger).
    /// Re-sorts `working` by timestamp at the end so paged-
    /// in content lands at its original chronological
    /// position rather than at the back where it was just
    /// pushed.
    /// Embed the latest user prompt in `working`, caching
    /// per-turn keyed by the prompt's timestamp. Returns
    /// `None` when no embedder is configured, when the
    /// prompt has no text, or when the embed call fails
    /// (logged at warn-level by the caller's fallback
    /// path).
    async fn cached_or_compute_prompt_embedding(&self, working: &[Message]) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?;
        // Find the latest user message + its timestamp.
        let (text, ts) = working.iter().rev().find_map(|m| match m {
            Message::User(u) => first_text(&u.content).map(|t| (t.to_string(), u.timestamp)),
            _ => None,
        })?;
        // Cache hit: same timestamp as last fire.
        {
            let cache = self.cached_prompt_embed.lock().await;
            if let Some((cached_ts, vec)) = cache.as_ref() {
                if *cached_ts == ts {
                    return Some(vec.clone());
                }
            }
        }
        // Miss: embed and store.
        match embedder.embed(&text).await {
            Ok(vec) => {
                *self.cached_prompt_embed.lock().await = Some((ts, vec.clone()));
                Some(vec)
            }
            Err(error) => {
                tracing::warn!(
                    target: "anie_cli::context_virt",
                    %error,
                    "prompt embed failed; reranker falling back to keyword overlap"
                );
                None
            }
        }
    }

    async fn page_in_relevant(&self, working: &mut Vec<Message>) -> usize {
        if self.relevance_budget_tokens == 0 {
            return 0;
        }
        let Some(prompt_tokens) = current_prompt_tokens(working) else {
            return 0;
        };

        // Plan-08: if an embedder is configured, embed the
        // prompt once per turn (cached by latest-user-msg
        // timestamp) and use cosine similarity for
        // scoring when a candidate has a cached
        // embedding. Falls back to keyword overlap per-
        // candidate when either side is missing
        // embeddings.
        let prompt_embed = self.cached_or_compute_prompt_embedding(working).await;

        // Take a snapshot of evicted candidates outside any
        // lock so we don't hold the read guard while
        // scoring. For each candidate also clone its summary
        // and embedding so the budget loop can fall back to
        // summary form / keyword overlap as needed.
        let working_ts: HashSet<u64> = working.iter().map(message_timestamp).collect();
        let mut candidates: Vec<RelevanceCandidate> = {
            let external = self.external.read().await;
            external
                .iter_with_meta()
                .filter(|(_, m, _, _)| !working_ts.contains(&message_timestamp(m)))
                .filter_map(|(id, m, summary, embedding)| {
                    let s = score_candidate(prompt_embed.as_deref(), &prompt_tokens, embedding, m);
                    if s <= 0.0 {
                        None
                    } else {
                        Some(RelevanceCandidate {
                            score: s,
                            id,
                            message: m.clone(),
                            summary: summary.map(str::to_string),
                        })
                    }
                })
                .collect()
        };

        if candidates.is_empty() {
            return 0;
        }

        // Sort by score descending; tie-break by recency
        // (later timestamps preferred). NaN guard: if a
        // score ever ends up NaN (shouldn't with our
        // cosine guards), treat it as Equal so the sort
        // stays total.
        candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| message_timestamp(&b.message).cmp(&message_timestamp(&a.message)))
        });

        let mut budget = self.relevance_budget_tokens;
        let mut paged = 0;
        for candidate in candidates {
            let RelevanceCandidate {
                id,
                message,
                summary,
                ..
            } = candidate;
            // Prefer the full body when it fits. Fall back
            // to the summary if it doesn't and a summary is
            // available — this is the Phase F payoff for the
            // reranker. Skip entirely otherwise.
            let full_cost = estimate_tokens(&message);
            if full_cost <= budget {
                budget = budget.saturating_sub(full_cost);
                working.push(message);
                paged += 1;
            } else if let Some(summary_text) = summary {
                let summary_message = Message::User(UserMessage {
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[summary of archive entry {id} — full body paged out]\n\n{summary_text}"
                        ),
                    }],
                    timestamp: message_timestamp(&message),
                });
                let summary_cost = estimate_tokens(&summary_message);
                if summary_cost > budget {
                    continue;
                }
                budget = budget.saturating_sub(summary_cost);
                working.push(summary_message);
                paged += 1;
            }
            if budget == 0 {
                break;
            }
        }

        if paged > 0 {
            // Stable sort by timestamp so paged-in content
            // lands in chronological order alongside the
            // surviving FIFO content.
            working.sort_by_key(message_timestamp);
        }
        paged
    }

    /// Build the structured ledger as a `User` message
    /// wrapped in `<system-reminder>` tags. Counts come from
    /// the shared `ExternalContext` indexes; tool-result
    /// breakdown is sorted by frequency and capped at 8 names
    /// to keep the ledger bounded (target ≤500 tokens).
    async fn build_ledger(&self, active_len: usize, paged_in_count: usize) -> Message {
        let lines = {
            let external = self.external.read().await;
            let total = external.len();
            let evicted = total.saturating_sub(active_len);

            // Imperative header. Earlier versions said "use
            // the recurse tool to access evicted content" —
            // permissive language the model treated as
            // optional, leading to repeated re-fetches of
            // URLs already in the archive. The directive
            // form below is explicit: scan the lists, prefer
            // recurse over re-running tools whose targets
            // are already listed.
            let mut lines = vec![
                "<system-reminder>".to_string(),
                format!(
                    "external context — {total} archived messages ({evicted} evicted, {active_len} active)"
                ),
                String::new(),
                "Before issuing a new tool call, scan the lists below.".to_string(),
                "If the URL, query, command, or path you're about to use is already listed,"
                    .to_string(),
                "the result is in the archive — do NOT re-run the tool. Use `recurse` instead:"
                    .to_string(),
                "  - `scope.kind=message_grep`, `pattern=<regex>` — search archived messages"
                    .to_string(),
                "    by keyword. Easiest option; needs no id.".to_string(),
                "  - `scope.kind=tool_result`, `tool_call_id=<id>` — fetch one prior result"
                    .to_string(),
                "    verbatim. Each ledger entry is `<value> (id=<call_id>)`; pass the".to_string(),
                "    `<call_id>` (without the surrounding parens) as the tool_call_id.".to_string(),
                "  - `scope.kind=summary`, `id=<archive_id>` — fetch the gist. Cheapest."
                    .to_string(),
                "Re-running a tool whose output is already archived wastes user time.".to_string(),
                String::new(),
            ];

            if paged_in_count > 0 {
                lines.push(format!(
                    "- {paged_in_count} relevant prior messages paged in for this turn"
                ));
            }

            // Phase F: report how many archive entries the
            // background summarizer has produced. Lets the
            // model know summaries are available; future
            // recurse-side work will let it ask for the
            // summary form directly.
            let summarized = external.summary_count();
            if summarized > 0 {
                lines.push(format!(
                    "- {summarized} archive entries have summaries available"
                ));
            }

            // Plan 08: report embedded count too. Mostly
            // operator-facing — confirms the embed worker
            // is keeping up with archive growth.
            let embedded = external.embedding_count();
            if embedded > 0 {
                lines.push(format!(
                    "- {embedded} archive entries have embeddings (semantic relevance)"
                ));
            }

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

            // Tool-call identity summary (URLs / queries /
            // commands / paths). Without this the model can
            // see "I have 6 web_read results" but not "I
            // already fetched engineering.fyi/codex-harness"
            // — and re-issues the same fetch when the
            // result text was evicted. With it, the model
            // can short-circuit duplicate work directly from
            // the ledger.
            let summary = collect_tool_call_summary(&external);
            for line in render_tool_call_summary_lines(&summary) {
                lines.push(line);
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

/// PR 4 of `docs/harness_mitigations_2026-05-01/`. Walk
/// `working` and identify failed tool results that have
/// been "superseded" by a later successful tool call with
/// the same `(tool_name, args_hash)`. The args come from
/// the upstream assistant message's `ToolCall.arguments`
/// (matched by `tool_call_id`); when a tool's arguments are
/// unrecoverable (e.g., the assistant message was already
/// evicted), the failed result conservatively stays put.
///
/// Returns indices into `working`, sorted ascending.
fn find_supersedable_failures(working: &[Message]) -> Vec<usize> {
    use std::collections::{HashMap, HashSet};

    // 1. Map tool_call_id → arguments JSON by walking the
    //    assistant messages' tool-call blocks. We keep the
    //    full Value so we can hash it once we know the
    //    matching tool result.
    let mut args_by_call_id: HashMap<&str, &serde_json::Value> = HashMap::new();
    for m in working {
        if let Message::Assistant(a) = m {
            for block in &a.content {
                if let ContentBlock::ToolCall(call) = block {
                    args_by_call_id.insert(call.id.as_str(), &call.arguments);
                }
            }
        }
    }

    // 2. Collect the (tool_name, args_hash) pairs of
    //    successful tool results in `working`. These mark
    //    "supersession keys" — any failed result with a
    //    matching key is redundant.
    let mut success_keys: HashSet<(String, u64)> = HashSet::new();
    for m in working {
        if let Message::ToolResult(tr) = m
            && !tr.is_error
            && let Some(args) = args_by_call_id.get(tr.tool_call_id.as_str())
        {
            success_keys.insert((tr.tool_name.clone(), stable_args_hash(args)));
        }
    }

    // 3. Walk `working` again and collect indices of failed
    //    results whose key is in `success_keys`.
    let mut supersedable = Vec::new();
    for (idx, m) in working.iter().enumerate() {
        if let Message::ToolResult(tr) = m
            && tr.is_error
            && let Some(args) = args_by_call_id.get(tr.tool_call_id.as_str())
            && success_keys.contains(&(tr.tool_name.clone(), stable_args_hash(args)))
        {
            supersedable.push(idx);
        }
    }
    supersedable
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_protocol::{
        AssistantMessage, ContentBlock, Message, StopReason, ToolCall, ToolResultMessage, Usage,
        UserMessage,
    };
    use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat};

    /// Build an Assistant message that issues `tool_name`
    /// with the given JSON arguments. Used by the ledger
    /// enrichment tests to populate the archive with
    /// tool-call identity info the model would have seen.
    fn assistant_with_tool_call(tool_name: &str, arguments: serde_json::Value, ts: u64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: format!("call_{ts}"),
                name: tool_name.into(),
                arguments,
            })],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "test".into(),
            model: "test".into(),
            timestamp: ts,
            reasoning_details: None,
        })
    }

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
        let policy = ContextVirtualizationPolicy::new(u64::MAX, 4, 0, store, HashSet::new());
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
        let policy = ContextVirtualizationPolicy::new(10_000, 4, 0, store, HashSet::new());
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
        let policy = ContextVirtualizationPolicy::new(5, 3, 0, store, HashSet::new());
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

    /// rlm/17: the latest User message must always be
    /// preserved. Without this pinning, tight ceilings can
    /// evict the user's directive itself, leading the model
    /// to confabulate a task from contextual cues.
    /// (Observed: qwen3.5:9b under 1.5k ceiling +
    /// KEEP_LAST_N=2 invented a fix narrative for a struct
    /// that doesn't exist.)
    #[tokio::test]
    async fn latest_user_message_survives_aggressive_eviction() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // Build a context where, with KEEP_LAST_N=2 and a
        // tight ceiling, ordinary FIFO eviction would
        // evict the user prompt at position 0.
        let context = vec![
            user("Just say done.", 1), // the directive — must survive
            assistant("ok let me read", 2),
            tool_result("c1", "read", "lots of file content here", 3),
            assistant("read result", 4),
            tool_result("c2", "read", "more file content", 5),
            assistant("another read", 6),
            tool_result("c3", "read", "final file content", 7),
        ];
        // Tiny ceiling forces eviction; KEEP_LAST_N=2 means
        // only the last 2 messages would normally pin.
        let policy = ContextVirtualizationPolicy::new(2, 2, 0, store, HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };

        // The user prompt should be in survivors despite
        // tight ceiling.
        let has_user_directive = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text == "Just say done.",
                _ => false,
            },
            _ => false,
        });
        assert!(
            has_user_directive,
            "user's directive must survive eviction; got {survivors:?}"
        );
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
        let policy = ContextVirtualizationPolicy::new(1, 5, 0, store, HashSet::new());
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
        let policy = ContextVirtualizationPolicy::new(5, 2, 0, Arc::clone(&store), HashSet::new());
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
        let policy = ContextVirtualizationPolicy::new(5, 2, 0, Arc::clone(&external), pushed);
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
        let policy = ContextVirtualizationPolicy::new(5, 2, 0, Arc::clone(&store), HashSet::new());
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
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, store, HashSet::new());

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
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, store, HashSet::new());
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
        assert!(ledger_text.contains("recurse"));
        // Imperative directive must be present — this is
        // the rlm/14 anti-re-fetch fix.
        assert!(
            ledger_text.contains("do NOT re-run the tool"),
            "ledger should explicitly forbid re-running tools: {ledger_text}",
        );
        assert!(ledger_text.contains("5 archived messages"));
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
            ContextVirtualizationPolicy::new(10_000, 8, 0, Arc::clone(&store), HashSet::new());
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

    fn user_text(m: &Message) -> Option<&str> {
        match m {
            Message::User(u) => match u.content.first()? {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Tokenizer drops common stopwords + sub-3-char tokens
    /// + collapses to lowercase.
    ///
    /// Tokens differing only in case land in the same bucket;
    /// "the" is filtered.
    #[test]
    fn tokenize_filters_stopwords_and_short_tokens() {
        let toks = tokenize("The quick brown fox jumps over the lazy dog");
        // "the", "over" → stopwords; "fox", "dog" → kept.
        assert!(!toks.contains("the"));
        assert!(!toks.contains("over"));
        assert!(toks.contains("quick"));
        assert!(toks.contains("brown"));
        assert!(toks.contains("fox"));
        assert!(toks.contains("dog"));
        assert!(toks.contains("jumps"));
        assert!(toks.contains("lazy"));
    }

    /// Tokenizer is case-insensitive and splits on
    /// non-alphanumerics.
    #[test]
    fn tokenize_normalizes_case_and_splits_punctuation() {
        let toks = tokenize("Tallahassee, FL — weather forecast?");
        assert!(toks.contains("tallahassee"));
        assert!(toks.contains("weather"));
        assert!(toks.contains("forecast"));
    }

    /// `score_message`: intersection size between prompt
    /// tokens and message tokens. Stopwords don't count.
    #[test]
    fn score_message_returns_intersection_size() {
        let prompt_tokens = tokenize("weather forecast Tallahassee");
        let m = user("the weather in Tallahassee is sunny", 1);
        // "weather" + "tallahassee" overlap; "the" is a
        // stopword.
        assert_eq!(score_message(&prompt_tokens, &m), 2);
        let unrelated = user("hello world friends", 1);
        assert_eq!(score_message(&prompt_tokens, &unrelated), 0);
    }

    /// Phase E: with `relevance_budget_tokens = 0`, the
    /// policy never pages in. Equivalent to Phase C
    /// behavior. Sets up an evicted message that would be
    /// highly relevant; verifies it stays evicted.
    #[tokio::test]
    async fn relevance_budget_zero_disables_paging() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..6)
            .map(|i| user(&format!("evictable msg{i}"), i as u64))
            .chain([user("weather forecast for Tallahassee tomorrow", 100)])
            .collect();
        // Budget = 0; ceiling = 5 forces eviction.
        let policy = ContextVirtualizationPolicy::new(5, 1, 0, store, HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Expected: pinned tail (1 most-recent) + ledger.
        // No paging happened — survivors.len() == 2.
        assert_eq!(survivors.len(), 2);
    }

    /// Phase E: with a relevance budget, evicted messages
    /// matching the prompt's keywords get paged back in.
    /// Sets up a 10-message context with a topical
    /// keyword-match buried in the front; tight ceiling
    /// evicts it; relevance budget pages it back in.
    #[tokio::test]
    async fn paged_in_messages_match_prompt_keywords() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // The buried message contains the keyword
        // "Tallahassee"; the rest are unrelated chatter
        // that should not match.
        let mut context: Vec<Message> = vec![
            user(
                "here's a long discussion about Tallahassee weather patterns",
                1,
            ),
            user("filler about pets", 2),
            user("filler about food", 3),
            user("filler about music", 4),
            user("filler about books", 5),
            user("filler about movies", 6),
            user("filler about sports", 7),
        ];
        // Current prompt — last user message — asks about
        // Tallahassee.
        context.push(user("what's the weather in Tallahassee tomorrow?", 100));

        // Tight ceiling forces eviction of message 1; budget
        // big enough to page it back.
        let policy = ContextVirtualizationPolicy::new(5, 1, 50, Arc::clone(&store), HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };

        // Find the Tallahassee message in survivors. With
        // FIFO+keep_last_n=1 alone it would be evicted; the
        // reranker should have paged it back in.
        let has_tallahassee_match = survivors.iter().any(|m| {
            user_text(m)
                .map(|t| t.contains("Tallahassee weather patterns"))
                .unwrap_or(false)
        });
        assert!(
            has_tallahassee_match,
            "relevance reranker should have paged in the Tallahassee message"
        );
    }

    /// Phase E: paging-in respects the budget. Many
    /// matching candidates, small budget — only the highest
    /// scorers fit. Sum of paged-in message tokens ≤ budget.
    #[tokio::test]
    async fn paged_in_respects_budget() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let mut context: Vec<Message> = (0..10)
            .map(|i| user(&format!("weather report number {i}"), i as u64))
            .collect();
        context.push(user("what's the weather like?", 100));

        let budget = 6_u64;
        let policy =
            ContextVirtualizationPolicy::new(2, 1, budget, Arc::clone(&store), HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Identify paged-in messages: present in survivors
        // and in the original context's evictable region
        // (positions 0..10), excluding the pinned tail
        // (position 10) and the ledger.
        let pinned_tail_ts = message_timestamp(&context[10]);
        let paged_in: Vec<&Message> = survivors
            .iter()
            .filter(|m| !is_ledger(m) && message_timestamp(m) != pinned_tail_ts)
            .collect();
        let paged_tokens: u64 = paged_in
            .iter()
            .map(|m| estimate_tokens(m))
            .fold(0, u64::saturating_add);
        assert!(
            paged_tokens <= budget,
            "paged-in tokens ({paged_tokens}) exceeded budget ({budget}); \
             {} message(s) were paged in",
            paged_in.len()
        );
        // Sanity: at least one was paged in (otherwise the
        // test isn't exercising the path).
        assert!(!paged_in.is_empty(), "expected at least one paged-in");
    }

    /// Phase E: paging-in does not duplicate messages that
    /// are already in `working`. The reranker filters
    /// candidates by timestamp absence in working.
    #[tokio::test]
    async fn paged_in_excludes_active_context() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // All 5 messages contain "weather" (high score
        // candidates). With keep_last_n = 5 and ceiling
        // 10_000, no eviction triggers, so ALL 5 are
        // already in working.
        let context: Vec<Message> = (0..5)
            .map(|i| user(&format!("weather weather {i}"), i as u64))
            .chain([user("what's the weather?", 100)])
            .collect();
        let policy =
            ContextVirtualizationPolicy::new(10_000, 6, 1_000, Arc::clone(&store), HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Survivors count = original count + ledger; no
        // duplicates (no message appears twice).
        assert_eq!(survivors.len(), context.len() + 1);
        let originals: Vec<&Message> = survivors.iter().filter(|m| !is_ledger(m)).collect();
        let ts_set: HashSet<u64> = originals.iter().map(|m| message_timestamp(m)).collect();
        assert_eq!(
            ts_set.len(),
            originals.len(),
            "no message should appear twice in working"
        );
    }

    /// Phase E: paged-in messages land in chronological
    /// order. After paging, working is sorted by
    /// timestamp — the model sees a coherent timeline
    /// rather than reranker output bolted on at the back.
    #[tokio::test]
    async fn paged_in_chronologically_ordered() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let mut context: Vec<Message> = (0..8)
            .map(|i| user(&format!("topic{i} weather"), i as u64))
            .collect();
        context.push(user("weather question", 100));

        let policy =
            ContextVirtualizationPolicy::new(2, 1, 1_000, Arc::clone(&store), HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let originals: Vec<&Message> = survivors.iter().filter(|m| !is_ledger(m)).collect();
        // Timestamps in originals must be non-decreasing.
        for w in originals.windows(2) {
            let a = message_timestamp(w[0]);
            let b = message_timestamp(w[1]);
            assert!(a <= b, "timestamps out of order: {a} appeared before {b}");
        }
    }

    /// Phase E: ledger reports the paged-in count when
    /// non-zero.
    #[tokio::test]
    async fn ledger_reports_paged_in_count() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let mut context: Vec<Message> = (0..6)
            .map(|i| user(&format!("weather chat {i}"), i as u64))
            .collect();
        context.push(user("weather!", 100));

        let policy =
            ContextVirtualizationPolicy::new(2, 1, 1_000, Arc::clone(&store), HashSet::new());
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ledger_text = survivors
            .iter()
            .find_map(|m| {
                if is_ledger(m) {
                    user_text(m).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .expect("ledger present");
        assert!(
            ledger_text.contains("paged in for this turn"),
            "expected paged-in count in ledger: {ledger_text}"
        );
    }

    /// Tool-call summary appears in the ledger so the model
    /// can see which URLs / queries / commands have already
    /// been issued and avoid duplicate fetches. This is the
    /// fix for the user-reported re-read loop.
    #[test]
    fn collect_tool_call_summary_lists_urls_queries_commands_paths() {
        let store = ExternalContext::from_messages(vec![
            assistant_with_tool_call(
                "web_read",
                serde_json::json!({"url": "https://engineering.fyi/codex-harness"}),
                1,
            ),
            assistant_with_tool_call(
                "web_read",
                serde_json::json!({"url": "https://deepwiki.com/opencode/2.4"}),
                2,
            ),
            // Duplicate URL — should not appear twice.
            assistant_with_tool_call(
                "web_read",
                serde_json::json!({"url": "https://engineering.fyi/codex-harness"}),
                3,
            ),
            assistant_with_tool_call(
                "web_search",
                serde_json::json!({"query": "Codex agent loop architecture"}),
                4,
            ),
            assistant_with_tool_call(
                "bash",
                serde_json::json!({"command": "cargo test --workspace"}),
                5,
            ),
            assistant_with_tool_call("read", serde_json::json!({"path": "src/main.rs"}), 6),
        ]);

        let summary = collect_tool_call_summary(&store);
        let summary_map: HashMap<String, Vec<ToolCallEntry>> = summary.into_iter().collect();

        // Entries surface the tool_call_id (assigned by
        // `assistant_with_tool_call` test helper as
        // "call_<ts>") plus the truncated arg value.
        let web_read = summary_map.get("web_read").expect("web_read entries");
        assert_eq!(web_read.len(), 2, "duplicate URL must dedupe");
        assert_eq!(web_read[0].tool_call_id, "call_1");
        assert_eq!(
            web_read[0].arg_value,
            "https://engineering.fyi/codex-harness"
        );
        assert_eq!(web_read[1].tool_call_id, "call_2");
        assert_eq!(web_read[1].arg_value, "https://deepwiki.com/opencode/2.4");

        let web_search = summary_map.get("web_search").expect("web_search entries");
        assert_eq!(web_search[0].tool_call_id, "call_4");
        assert_eq!(web_search[0].arg_value, "Codex agent loop architecture");

        let bash = summary_map.get("bash").expect("bash entries");
        assert_eq!(bash[0].tool_call_id, "call_5");
        assert_eq!(bash[0].arg_value, "cargo test --workspace");

        let read = summary_map.get("read").expect("read entries");
        assert_eq!(read[0].tool_call_id, "call_6");
        assert_eq!(read[0].arg_value, "src/main.rs");
    }

    /// `truncate_for_ledger` shortens overlong values + adds
    /// the ellipsis so a single 500-char URL doesn't blow
    /// past the ledger token target.
    #[test]
    fn truncate_for_ledger_caps_long_values() {
        let s = "a".repeat(200);
        let truncated = truncate_for_ledger(&s, 80);
        assert_eq!(truncated.chars().count(), 80);
        assert!(truncated.ends_with('…'));
        // Short values pass through unchanged.
        assert_eq!(truncate_for_ledger("short", 80), "short");
    }

    /// Ledger output includes the URL/query lines so the
    /// model can see what's already been fetched.
    #[tokio::test]
    async fn ledger_includes_tool_call_identities() {
        let store = ExternalContext::from_messages(vec![
            user("question about codex", 100),
            assistant_with_tool_call(
                "web_read",
                serde_json::json!({"url": "https://engineering.fyi/codex"}),
                101,
            ),
            tool_result("c1", "web_read", "page contents", 102),
            assistant_with_tool_call(
                "web_search",
                serde_json::json!({"query": "Codex architecture"}),
                103,
            ),
        ]);
        let store = Arc::new(RwLock::new(store));

        // Use under-ceiling pipeline; ledger is built either
        // way. Pre-populate `pushed` so we don't re-archive.
        let pushed: HashSet<u64> = (100..=103).collect();
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, Arc::clone(&store), pushed);
        let context = vec![user("follow-up about codex", 200)];
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ledger_text = survivors
            .iter()
            .find_map(|m| {
                if is_ledger(m) {
                    user_text(m).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .expect("ledger present");

        assert!(
            ledger_text.contains("web_read targets:"),
            "ledger should list web_read URLs: {ledger_text}"
        );
        assert!(
            ledger_text.contains("https://engineering.fyi/codex"),
            "ledger should include the actual URL: {ledger_text}"
        );
        assert!(
            ledger_text.contains("web_search queries:"),
            "ledger should list web_search queries: {ledger_text}"
        );
        assert!(
            ledger_text.contains("Codex architecture"),
            "ledger should include the query text: {ledger_text}"
        );
    }

    /// Ledger caps each tool's identity list at
    /// `TOOL_CALL_DISPLAY_CAP` and reports overflow with a
    /// "+N more" suffix. Keeps the ledger bounded even when
    /// the agent has fired hundreds of tool calls.
    #[test]
    fn render_tool_call_summary_truncates_with_more_suffix() {
        let entries: Vec<ToolCallEntry> = (0..12)
            .map(|i| ToolCallEntry {
                tool_call_id: format!("tc_{i}"),
                arg_value: format!("https://example.com/page{i}"),
            })
            .collect();
        let summary = vec![("web_read".to_string(), entries)];
        let lines = render_tool_call_summary_lines(&summary);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(
            line.contains("+4 more"),
            "expected +4 more suffix when 12 entries against cap 8: {line}"
        );
        // First 8 entries appear with their ids; entry #9
        // (page8 / tc_8) does not. New format is
        // `<value> (id=<call_id>)`.
        assert!(line.contains("page0 (id=tc_0)"));
        assert!(line.contains("page7 (id=tc_7)"));
        assert!(!line.contains("page8"));
        assert!(!line.contains("(id=tc_8)"));
    }

    /// New test: the rendered ledger lines surface the
    /// `tool_call_id` so the model can use
    /// `RecurseScope::ToolResult` without inventing ids.
    #[test]
    fn render_tool_call_summary_includes_real_tool_call_ids() {
        let entries = vec![
            ToolCallEntry {
                tool_call_id: "ollama_tool_call_8_2".into(),
                arg_value: "https://weather.gov/Tallahassee".into(),
            },
            ToolCallEntry {
                tool_call_id: "ollama_tool_call_8_3".into(),
                arg_value: "https://weather.com/Tifton".into(),
            },
        ];
        let summary = vec![("web_read".to_string(), entries)];
        let lines = render_tool_call_summary_lines(&summary);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(
            line.contains("https://weather.gov/Tallahassee (id=ollama_tool_call_8_2)"),
            "value should be first, id in parens after: {line}"
        );
        assert!(line.contains("https://weather.com/Tifton (id=ollama_tool_call_8_3)"));
    }

    /// Phase F reranker integration: when an evicted
    /// candidate's full body is too large for the relevance
    /// budget but its summary fits, the reranker pages the
    /// summary in instead of skipping the candidate
    /// entirely. The summary message gets a clear header
    /// so the model knows it's looking at a summary rather
    /// than the original.
    #[tokio::test]
    async fn reranker_falls_back_to_summary_when_full_body_too_big() {
        // Build a large message that exceeds the relevance
        // budget. Repeat a keyword the prompt will match.
        let huge_text: String = "relevant_keyword data ".repeat(200);
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            &huge_text, 1,
        )])));
        store
            .write()
            .await
            .set_summary(0, "concise summary text".to_string());

        // Tight budget — full body won't fit, summary
        // will. Active context references the keyword so
        // the reranker scores the candidate.
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            150, // budget tight enough that huge body skips, summary fits
            Arc::clone(&store),
            HashSet::from([1u64]), // skip re-archive
        );
        let context = vec![user("looking for relevant_keyword info", 200)];
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };

        // The summary should be paged in, not the huge
        // body. Find a User message containing the
        // "[summary of archive entry" header.
        let has_summary = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => {
                    text.starts_with("[summary of archive entry")
                        && text.contains("concise summary text")
                }
                _ => false,
            },
            _ => false,
        });
        assert!(
            has_summary,
            "expected summary fallback in survivors; got {survivors:?}"
        );
        // The huge body should NOT be in survivors —
        // budget too tight.
        let has_huge = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text.contains(&huge_text),
                _ => false,
            },
            _ => false,
        });
        assert!(
            !has_huge,
            "huge body should not have been paged in alongside the summary"
        );
    }

    /// Plan 08: when the policy has an embedder + the
    /// candidate has a cached embedding, the reranker
    /// scores by cosine similarity. Verify a high-cosine
    /// candidate gets paged in even when its keyword
    /// overlap with the prompt is zero.
    #[tokio::test]
    async fn reranker_prefers_high_cosine_similarity() {
        use crate::bg_embedder::EmbedRequest;

        // Build store with one candidate. Its content has
        // no keyword overlap with the prompt, but its
        // embedding will match the prompt's exactly.
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "zero keyword overlap content xyz",
            1,
        )])));
        // Pre-set the embedding directly (skip the
        // worker) so the test is deterministic.
        store.write().await.set_embedding(0, vec![1.0, 0.0, 0.0]);

        // Stub embedder: prompt embeds to the same
        // vector as the candidate.
        struct StubEmbedder;
        #[async_trait::async_trait]
        impl crate::embedder::Embedder for StubEmbedder {
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
                Ok(vec![1.0, 0.0, 0.0])
            }
            fn dim(&self) -> usize {
                3
            }
        }
        let embedder: Arc<dyn crate::embedder::Embedder> = Arc::new(StubEmbedder);
        let (tx, _rx) = mpsc::channel::<EmbedRequest>(8);

        let pushed = HashSet::from([1u64]);
        let policy =
            ContextVirtualizationPolicy::new(10_000, 2, 10_000, Arc::clone(&store), pushed)
                .with_embedder(embedder, tx);

        // Active context's prompt has no keyword overlap
        // with the candidate. With keyword scoring this
        // would page in nothing; with embedding cosine=1
        // it should page in.
        let context = vec![user("totally different abc query", 100)];
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let has_candidate = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text.contains("zero keyword overlap"),
                _ => false,
            },
            _ => false,
        });
        assert!(
            has_candidate,
            "embedding cosine=1 candidate should have been paged in: {survivors:?}"
        );
    }

    /// Plan 08: when the candidate has no cached
    /// embedding (worker behind), fall back to keyword
    /// overlap for that candidate even if other
    /// candidates use embeddings. Verifies the per-
    /// candidate fallback works.
    #[tokio::test]
    async fn reranker_falls_back_to_keyword_when_no_embedding() {
        use crate::bg_embedder::EmbedRequest;

        // Two candidates: one embedded, one not. Prompt
        // has keyword overlap with the unembedded one.
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            user("foo bar baz quux", 1), // unembedded, keyword "quux" matches prompt
            user("entirely unrelated content here", 2), // embedded
        ])));
        store.write().await.set_embedding(1, vec![1.0, 0.0, 0.0]);

        struct OrthogonalEmbedder;
        #[async_trait::async_trait]
        impl crate::embedder::Embedder for OrthogonalEmbedder {
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
                // Orthogonal to candidate 1's embedding.
                Ok(vec![0.0, 1.0, 0.0])
            }
            fn dim(&self) -> usize {
                3
            }
        }
        let embedder: Arc<dyn crate::embedder::Embedder> = Arc::new(OrthogonalEmbedder);
        let (tx, _rx) = mpsc::channel::<EmbedRequest>(8);

        let pushed = HashSet::from([1u64, 2u64]);
        let policy =
            ContextVirtualizationPolicy::new(10_000, 2, 10_000, Arc::clone(&store), pushed)
                .with_embedder(embedder, tx);

        let context = vec![user("looking for quux", 100)];
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Candidate 0 (foo bar baz quux) should be paged
        // in by keyword fallback (orthogonal embedding
        // means cosine=0 for the embedded candidate).
        let has_keyword_match = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text.contains("foo bar baz quux"),
                _ => false,
            },
            _ => false,
        });
        assert!(
            has_keyword_match,
            "keyword-overlap candidate should fall through despite presence of embeddings: {survivors:?}"
        );
    }

    /// Plan 08: when no embedder is configured, behavior
    /// matches the pre-08 keyword-overlap reranker
    /// exactly. Same setup as
    /// `paged_in_messages_match_prompt_keywords` but
    /// double-checked with this guard.
    #[tokio::test]
    async fn reranker_falls_back_to_keyword_when_embedder_unconfigured() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "relevant_keyword content here",
            1,
        )])));
        // No `with_embedder` call → reranker uses
        // keyword overlap exclusively.
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            2,
            10_000,
            Arc::clone(&store),
            HashSet::from([1u64]),
        );
        let context = vec![user("looking for relevant_keyword", 100)];
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let has_candidate = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text.contains("relevant_keyword"),
                _ => false,
            },
            _ => false,
        });
        assert!(has_candidate, "keyword-only path should still page in");
    }

    // ---- PR 4 of harness_mitigations_2026-05-01: supersedable failure detection ----

    fn failed_tool_result(call_id: &str, tool_name: &str, ts: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text { text: "[tool error] failed".into() }],
            details: serde_json::Value::Null,
            is_error: true,
            timestamp: ts,
        })
    }

    fn ok_tool_result(call_id: &str, tool_name: &str, ts: u64) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: call_id.into(),
            tool_name: tool_name.into(),
            content: vec![ContentBlock::Text { text: "ok".into() }],
            details: serde_json::Value::Null,
            is_error: false,
            timestamp: ts,
        })
    }

    /// Helper: build an Assistant message whose tool-call has
    /// a specific id (so we can wire it to a matching tool
    /// result).
    fn assistant_call_with_id(
        id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
        ts: u64,
    ) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: id.into(),
                name: tool_name.into(),
                arguments,
            })],
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            provider: "test".into(),
            model: "test".into(),
            timestamp: ts,
            reasoning_details: None,
        })
    }

    #[test]
    fn supersedable_failure_detected_when_args_match_later_success() {
        let working = vec![
            assistant_call_with_id("c1", "bash", serde_json::json!({"command": "ls"}), 1),
            failed_tool_result("c1", "bash", 2),
            assistant_call_with_id("c2", "bash", serde_json::json!({"command": "ls"}), 3),
            ok_tool_result("c2", "bash", 4),
        ];
        let supersedable = find_supersedable_failures(&working);
        assert_eq!(supersedable, vec![1], "only the failed result at idx 1 supersedable");
    }

    #[test]
    fn no_supersedable_when_failure_args_differ_from_success() {
        let working = vec![
            assistant_call_with_id("c1", "bash", serde_json::json!({"command": "ls /a"}), 1),
            failed_tool_result("c1", "bash", 2),
            assistant_call_with_id("c2", "bash", serde_json::json!({"command": "ls /b"}), 3),
            ok_tool_result("c2", "bash", 4),
        ];
        let supersedable = find_supersedable_failures(&working);
        assert!(supersedable.is_empty(), "different args should not supersede");
    }

    #[test]
    fn no_supersedable_when_only_failures() {
        let working = vec![
            assistant_call_with_id("c1", "bash", serde_json::json!({"command": "ls"}), 1),
            failed_tool_result("c1", "bash", 2),
            assistant_call_with_id("c2", "bash", serde_json::json!({"command": "ls"}), 3),
            failed_tool_result("c2", "bash", 4),
        ];
        let supersedable = find_supersedable_failures(&working);
        assert!(supersedable.is_empty(), "no success → nothing to supersede");
    }

    #[test]
    fn supersedable_requires_same_tool_name() {
        let working = vec![
            assistant_call_with_id("c1", "bash", serde_json::json!({"command": "ls"}), 1),
            failed_tool_result("c1", "bash", 2),
            // Different tool, "same" args (irrelevant — different
            // tool means different concept).
            assistant_call_with_id("c2", "edit", serde_json::json!({"command": "ls"}), 3),
            ok_tool_result("c2", "edit", 4),
        ];
        let supersedable = find_supersedable_failures(&working);
        assert!(
            supersedable.is_empty(),
            "different tool_name should not match"
        );
    }

    #[test]
    fn supersedable_args_canonicalized_so_key_order_doesnt_matter() {
        let working = vec![
            assistant_call_with_id("c1", "bash", serde_json::json!({"a": 1, "b": 2}), 1),
            failed_tool_result("c1", "bash", 2),
            // Same args, different key order.
            assistant_call_with_id("c2", "bash", serde_json::json!({"b": 2, "a": 1}), 3),
            ok_tool_result("c2", "bash", 4),
        ];
        let supersedable = find_supersedable_failures(&working);
        assert_eq!(
            supersedable,
            vec![1],
            "stable_args_hash should treat reordered keys as same"
        );
    }

    #[test]
    fn multiple_supersedable_failures_returned_in_order() {
        let working = vec![
            assistant_call_with_id("c1", "bash", serde_json::json!({"command": "ls"}), 1),
            failed_tool_result("c1", "bash", 2),
            assistant_call_with_id("c2", "bash", serde_json::json!({"command": "ls"}), 3),
            failed_tool_result("c2", "bash", 4),
            // Eventually a success with the same args
            // supersedes BOTH prior failures.
            assistant_call_with_id("c3", "bash", serde_json::json!({"command": "ls"}), 5),
            ok_tool_result("c3", "bash", 6),
        ];
        let supersedable = find_supersedable_failures(&working);
        assert_eq!(supersedable, vec![1, 3]);
    }
}
