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

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse};
use anie_protocol::{AgentEvent, ContentBlock, Message, UserMessage, now_millis};
use anie_session::estimate_tokens;
use tokio::sync::{RwLock, mpsc};

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
/// paging back in. Carries the score (intersection size with
/// prompt tokens), the archive entry's stable id (for the
/// summary-fallback annotation), the full message body, and
/// the optional pre-computed summary.
struct RelevanceCandidate {
    score: usize,
    id: crate::external_context::MessageId,
    message: Message,
    summary: Option<String>,
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
        }
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

        // Step 2.5 (Phase F): fan newly-archived messages
        // out to the background summarizer if one's wired
        // up. Skip messages below the size threshold —
        // they don't benefit from summarization. The queue
        // is bounded; if the worker is behind, `try_send`
        // returns Full and we drop the request rather than
        // blocking the model turn.
        if let Some(tx) = &self.summarizer_tx {
            for (id, message) in newly_archived {
                if estimate_tokens(&message) < crate::bg_summarizer::SUMMARIZE_MIN_TOKENS {
                    continue;
                }
                let _ = tx.try_send(crate::bg_summarizer::SummaryRequest { id, message });
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
        let evicted_count = idx;
        if idx > 0 {
            working.drain(..idx);
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
/// return a per-tool list of unique meaningful arg values
/// (URLs, queries, commands, paths). Used by `build_ledger`
/// to surface tool-call identity to the model.
fn collect_tool_call_summary(external: &ExternalContext) -> Vec<(String, Vec<String>)> {
    let assistant_ids = external.ids_by_kind(MessageKindLabel::Assistant);
    let mut by_tool: HashMap<String, Vec<String>> = HashMap::new();
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
                    .push(truncated);
            }
        }
    }
    let mut out: Vec<(String, Vec<String>)> = by_tool.into_iter().collect();
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

/// Render `(tool_name, args)` pairs into ledger lines like
/// `- web_read targets: foo, bar, baz`. Empty input yields
/// no lines so the ledger stays compact when nothing has
/// been called yet.
fn render_tool_call_summary_lines(summary: &[(String, Vec<String>)]) -> Vec<String> {
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
        let head: Vec<&str> = args.iter().take(display).map(String::as_str).collect();
        let suffix = if total > display {
            format!(", +{} more", total - display)
        } else {
            String::new()
        };
        lines.push(format!(
            "- {tool_name} {label}: {}{suffix}",
            head.join(", ")
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
    async fn page_in_relevant(&self, working: &mut Vec<Message>) -> usize {
        if self.relevance_budget_tokens == 0 {
            return 0;
        }
        let Some(prompt_tokens) = current_prompt_tokens(working) else {
            return 0;
        };

        // Take a snapshot of evicted candidates outside any
        // lock so we don't hold the read guard while
        // scoring. For each candidate also clone its summary
        // (if one's been written) so the budget loop can
        // fall back to summary form when the full body
        // wouldn't fit.
        let working_ts: HashSet<u64> = working.iter().map(message_timestamp).collect();
        let mut candidates: Vec<RelevanceCandidate> = {
            let external = self.external.read().await;
            external
                .iter_with_meta()
                .filter(|(_, m, _)| !working_ts.contains(&message_timestamp(m)))
                .filter_map(|(id, m, summary)| {
                    let s = score_message(&prompt_tokens, m);
                    if s == 0 {
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
        // (later timestamps preferred). The reranker
        // assumes more-recent matches are likelier to be
        // relevant.
        candidates.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
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
                "the result is in the archive — do NOT re-run the tool. Instead use one of:"
                    .to_string(),
                "  - `recurse` with `scope.kind=tool_result`, `tool_call_id=<id>` to fetch a"
                    .to_string(),
                "    specific prior result verbatim;".to_string(),
                "  - `recurse` with `scope.kind=summary`, `id=<archive_id>` for the gist;"
                    .to_string(),
                "  - `recurse` with `scope.kind=message_grep`, `pattern=<regex>` to search"
                    .to_string(),
                "    archived messages by keyword.".to_string(),
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
        let summary_map: HashMap<String, Vec<String>> = summary.into_iter().collect();

        assert_eq!(
            summary_map.get("web_read").map(Vec::as_slice),
            Some(
                &[
                    "https://engineering.fyi/codex-harness".to_string(),
                    "https://deepwiki.com/opencode/2.4".to_string(),
                ][..]
            ),
            "web_read URLs should be deduplicated and ordered"
        );
        assert_eq!(
            summary_map.get("web_search").map(Vec::as_slice),
            Some(&["Codex agent loop architecture".to_string()][..])
        );
        assert_eq!(
            summary_map.get("bash").map(Vec::as_slice),
            Some(&["cargo test --workspace".to_string()][..])
        );
        assert_eq!(
            summary_map.get("read").map(Vec::as_slice),
            Some(&["src/main.rs".to_string()][..])
        );
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
        let urls: Vec<String> = (0..12)
            .map(|i| format!("https://example.com/page{i}"))
            .collect();
        let summary = vec![("web_read".to_string(), urls)];
        let lines = render_tool_call_summary_lines(&summary);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(
            line.contains("+4 more"),
            "expected +4 more suffix when 12 entries against cap 8: {line}"
        );
        // First 8 entries appear; entry #9 (page8) does not.
        assert!(line.contains("page0"));
        assert!(line.contains("page7"));
        assert!(!line.contains("page8"));
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
}
