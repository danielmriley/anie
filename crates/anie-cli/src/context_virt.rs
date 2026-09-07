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
//! messages pinning a token-budgeted tail; (2) archives every snapshot
//! message to the shared [`ExternalContext`] so the recurse
//! tool can still reach evicted content; (3) scores evicted
//! content against the current user prompt via keyword
//! overlap and recalls the highest scorers — within
//! `relevance_budget_tokens` and the per-run budget — into
//! one consolidated archive-recall message (rlm2/PR4);
//! (4) builds a structured ledger; (5) returns
//! `BeforeModelResponse::ReplaceMessages(working + recall +
//! ledger)`.
//!
//! Pinning rules (eviction-resistant):
//! - The latest `User` message is always preserved
//!   (rlm/17). Without this, tight ceilings can evict the
//!   user's directive itself, leading the model to
//!   confabulate a task from contextual cues.
//! - The latest `Assistant` message is always preserved
//!   (rlm2/PR5) — the model's own most recent reasoning.
//! - A token-budgeted trailing window is preserved
//!   (rlm2/PR5): the longest suffix whose estimated total
//!   fits `pin_tail_tokens` (default 3_072; the deprecated
//!   positional `ANIE_KEEP_LAST_N` converts at 512
//!   tokens/message). The trailing message is always
//!   pinned regardless of size. Protects turn continuity
//!   (current prompt + recent assistant/tool work) by cost
//!   rather than by count.
//!
//! These pins compose: the pinned user/assistant messages
//! can be at any position; the pinned tail is always the
//! trailing window. When the pins together exceed the
//! ceiling, the policy stops evicting and accepts being
//! over budget — correctness over budget compliance.
//!
//! Eviction order (rlm2/PR5): supersedable failures, then
//! stale failures, then the oldest LARGE tool results
//! (> 1_024 estimated tokens), then standard FIFO — one
//! stale file dump frees more ceiling than dozens of small
//! narrative texts.
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
//! enough to run on every fire. Selection is summaries-first
//! (rlm2/PR4): a scored candidate contributes its Phase-F
//! summary when one exists; its full body only when no
//! summary exists and the body is small. The recurse tool
//! covers the fidelity gap.
//!
//! Identity / dedup: messages are tracked by `timestamp`. The
//! agent loop generates one message per `now_millis()`
//! sample; collisions in practice are vanishingly rare. The
//! pushed-set is session-scoped and shared with the
//! controller's `RlmSessionState` (seeded once from the
//! session-start snapshot), so successive runs against the
//! same store never re-push messages it already holds.
//!
//! Default behavior: with `active_ceiling_tokens = u64::MAX`
//! the policy is effectively a noop — it returns `Continue`
//! on every call (no ledger, no eviction, no paging). The
//! controller installs the policy in `--harness-mode=rlm`;
//! default builds keep the noop policy. Setting
//! `ANIE_ACTIVE_CEILING_TOKENS` to a finite value turns on
//! the full eviction + ledger + relevance pipeline.
//!
//! Hysteresis (rlm2/PR3, `docs/rlm_context_v2/03_hysteresis.md`):
//! eviction is batched — when the running total breaches the
//! ceiling, the policy evicts down to a low-water mark
//! (`ceiling × ANIE_EVICT_LOW_WATER_PCT`, default 0.6) in one
//! pass, so the turns that follow append without evicting.
//! Append-only turns take a no-op fast path: when nothing was
//! evicted or paged in and the rebuilt ledger (and rebuilt
//! archive-recall message, when one exists) would be
//! byte-identical to what's already in the context, the
//! policy returns `Continue` — the existing messages stay in
//! place (the prompt prefix is a pure extension of the
//! previous turn's, so Ollama's prefix cache holds) and only
//! the store-side archiving of new messages runs.
//!
//! Page-in v2 (rlm2/PR4, `docs/rlm_context_v2/04_page_in_v2.md`):
//! recalled archive content is summaries-first — the reranker
//! pages in the Phase-F summary when one exists; full bodies
//! page in only when no summary exists AND the body is under
//! [`PAGE_IN_BODY_MAX_TOKENS`] (`ANIE_PAGE_IN_BODIES=1`
//! restores the old bodies-preferred behavior for A/B). All
//! recalled content renders inside ONE consolidated
//! `<system-reminder source="archive-recall">` message placed
//! immediately before the ledger — never interleaved into the
//! transcript as fake user turns. Recalled items are sticky:
//! keyed by the latest user prompt's timestamp, an item stays
//! in the recall message (and is never re-paged) until the
//! prompt changes; because recalled content never enters the
//! working set, it is structurally exempt from FIFO eviction.
//! Total page-in spend per run is capped by
//! `ANIE_PAGE_IN_RUN_BUDGET` (default 8192 tokens); the
//! counter lives in session-scoped [`PageInRunState`] so
//! continuation runs (same prompt, fresh per-run policy)
//! share it, and it resets when the prompt timestamp changes.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use anie_agent::{BeforeModelPolicy, BeforeModelRequest, BeforeModelResponse, stable_args_hash};
use anie_protocol::{AgentEvent, AssistantMessage, ContentBlock, Message, UserMessage, now_millis};
use anie_session::estimate_tokens;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

use crate::external_context::{
    ExternalContext, MessageId, MessageKindLabel, StoredMessage, first_text, first_text_of,
    tokenize,
};

/// Extract a Message's timestamp, regardless of variant.
fn message_timestamp(m: &Message) -> u64 {
    match m {
        Message::User(u) => u.timestamp,
        Message::Assistant(a) => a.timestamp,
        Message::ToolResult(t) => t.timestamp,
        Message::Custom(c) => c.timestamp,
    }
}

/// rlm2/PR4: is this message the policy's consolidated
/// archive-recall reminder? Content guard used alongside the
/// `last_recall_ts` timestamp match when stripping, so a
/// same-millisecond collision with the ledger (both are
/// `now_millis()`-stamped in the same fire) can't capture the
/// wrong message.
fn is_archive_recall(m: &Message) -> bool {
    first_text_of(m).is_some_and(|t| t.starts_with(ARCHIVE_RECALL_OPEN))
}

/// rlm2 review fix: policy-injected reminders (the repo map, the
/// ledger, the archive-recall message) ride the `User` variant on
/// the wire but are NOT the user's directive. The repo-map policy
/// in particular re-appends its `<system-reminder source="repo-
/// map">` message with a fresh timestamp AFTER the real prompt, so
/// any "latest user message" lookup that doesn't skip reminders
/// targets the repo map instead of the user's question — wrong
/// eviction pin, wrong reranker keywords, wrong sticky page-in
/// key. The ledger/recall are stripped before these run; this
/// guard covers every other reminder-shaped injection.
fn is_reminder_user(u: &UserMessage) -> bool {
    first_text(&u.content).is_some_and(|t| t.trim_start().starts_with("<system-reminder"))
}

/// Tokenize the most recent real `User` message in `working`
/// (policy-injected reminders skipped) — our proxy for "the
/// model's current request." Returns `None` when no user message
/// has any text content (rare; e.g., images-only) so the reranker
/// can short-circuit.
fn current_prompt_tokens(working: &[Message]) -> Option<HashSet<String>> {
    for m in working.iter().rev() {
        if let Message::User(u) = m {
            if is_reminder_user(u) {
                continue;
            }
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

/// The latest real `User` message's first text block + its
/// timestamp (policy-injected reminders skipped) — the input to
/// the prompt-embedding cache and the sticky page-in key. `None`
/// when no user message carries text (e.g., images-only).
fn latest_user_prompt(working: &[Message]) -> Option<(String, u64)> {
    working.iter().rev().find_map(|m| match m {
        Message::User(u) if !is_reminder_user(u) => {
            first_text(&u.content).map(|t| (t.to_string(), u.timestamp))
        }
        _ => None,
    })
}

/// One candidate the relevance reranker is considering for
/// paging back in. Carries the score (cosine similarity
/// when embeddings are available, keyword-overlap intersection
/// size cast to f32 otherwise), the archive entry's stable id
/// (for the summary-fallback annotation), and BORROWED views
/// of the message body and optional summary. rlm2/PR5 (perf):
/// candidates borrow from the store for the whole
/// score-and-select pass; the only owned data the reranker
/// produces is the rendered section text of selected items —
/// unselected bodies are never cloned.
struct RelevanceCandidate<'a> {
    /// Cosine similarity in [-1, 1] when scored by
    /// embedding; intersection-size as f32 when scored by
    /// keyword overlap. Higher is better for both — we
    /// can sort uniformly without normalizing because the
    /// fallback only fires per-candidate (we never mix
    /// scores in the same sort).
    score: f32,
    id: MessageId,
    message: &'a Message,
    summary: Option<&'a str>,
}

/// Score a stored candidate using the best available
/// signal. If both the prompt and the candidate have
/// embeddings, returns cosine similarity (range [-1, 1]).
/// Otherwise falls back to keyword overlap — the size of
/// the intersection between the prompt's tokens and the
/// candidate's token set cached at archive time (rlm2/PR5;
/// previously every fire re-tokenized every candidate
/// body). The two scales aren't commensurate but they only
/// ever appear in the same sort when the run is mid-warmup
/// (some candidates embedded, some not) — both are "higher
/// is better" so the relative ordering remains useful in
/// either regime.
fn score_candidate(
    prompt_embed: Option<&[f32]>,
    prompt_tokens: &HashSet<String>,
    candidate: &StoredMessage,
) -> f32 {
    if let (Some(p), Some(c)) = (prompt_embed, candidate.embedding.as_deref()) {
        return crate::embedder::cosine_similarity(p, c);
    }
    prompt_tokens.intersection(&candidate.tokens).count() as f32
}

/// rlm2/PR3: default low-water fraction for batch eviction.
/// Evicting to exactly the ceiling means the very next append
/// breaches it again — eviction (and the prefix-breaking
/// ledger rebuild) every turn. Evicting down to 60% of the
/// ceiling buys ~40% of the ceiling in append-only headroom.
const DEFAULT_EVICT_LOW_WATER_PCT: f64 = 0.6;

/// rlm2/PR3: parse the low-water fraction from an
/// `ANIE_EVICT_LOW_WATER_PCT` value. Clamped to [0.3, 0.9]:
/// below 0.3 a breach throws away most of the context;
/// above 0.9 the hysteresis band is too thin to matter.
/// Non-finite or unparseable values fall back to the default
/// (same convention as `resolve_rlm_active_ceiling_tokens`).
fn resolve_evict_low_water_pct(env_value: Option<&str>) -> f64 {
    let Some(raw) = env_value else {
        return DEFAULT_EVICT_LOW_WATER_PCT;
    };
    match raw.trim().parse::<f64>() {
        Ok(v) if v.is_finite() => v.clamp(0.3, 0.9),
        _ => DEFAULT_EVICT_LOW_WATER_PCT,
    }
}

/// `ANIE_EVICT_LOW_WATER_PCT`, parsed once per process.
fn env_evict_low_water_pct() -> f64 {
    static PCT: LazyLock<f64> = LazyLock::new(|| {
        resolve_evict_low_water_pct(std::env::var("ANIE_EVICT_LOW_WATER_PCT").ok().as_deref())
    });
    *PCT
}

/// rlm2/PR4: default per-run page-in spend cap. The old
/// per-turn-only budget let the navigation family re-page the
/// same bodies turn after turn (the 66k-token bill in the
/// corpus evidence); 8k tokens per run keeps recall useful
/// while bounding the total tax.
const DEFAULT_PAGE_IN_RUN_BUDGET_TOKENS: u64 = 8192;

/// rlm2/PR4: a body with no summary pages in only when it's
/// smaller than this. Anything larger is reachable via the
/// recurse tool (the ledger says so) — pushing it inline is
/// exactly the FIFO-displacement churn page-in v2 removes.
const PAGE_IN_BODY_MAX_TOKENS: u64 = 512;

/// rlm2/PR5: size-aware eviction threshold. Among evictable
/// messages, tool results estimated above this evict before
/// standard FIFO — a 4k-token file dump from twenty turns
/// ago costs more than every small assistant note combined,
/// while the small texts carry the narrative continuity.
const LARGE_TOOL_RESULT_EVICT_TOKENS: u64 = 1_024;

/// Opening tag of the consolidated archive-recall message.
/// The `source` attribute distinguishes it from the ledger's
/// plain `<system-reminder>` so the two can be stripped and
/// byte-compared independently.
const ARCHIVE_RECALL_OPEN: &str = "<system-reminder source=\"archive-recall\">";

/// rlm2/PR4: parse the per-run page-in budget from an
/// `ANIE_PAGE_IN_RUN_BUDGET` value. Unparseable values fall
/// back to the default (same convention as
/// `resolve_evict_low_water_pct`); 0 disables page-in for the
/// run entirely.
fn resolve_page_in_run_budget(env_value: Option<&str>) -> u64 {
    env_value
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_PAGE_IN_RUN_BUDGET_TOKENS)
}

/// `ANIE_PAGE_IN_RUN_BUDGET`, parsed once per process.
fn env_page_in_run_budget() -> u64 {
    static BUDGET: LazyLock<u64> = LazyLock::new(|| {
        resolve_page_in_run_budget(std::env::var("ANIE_PAGE_IN_RUN_BUDGET").ok().as_deref())
    });
    *BUDGET
}

/// rlm2/PR4: parse the `ANIE_PAGE_IN_BODIES` A/B escape hatch
/// — truthy restores the pre-PR4 bodies-preferred page-in
/// selection (full body when it fits the budget, summary as
/// the fallback). Same truthy set as the controller's
/// `env_flag_enabled`.
fn resolve_page_in_bodies(env_value: Option<&str>) -> bool {
    matches!(
        env_value.map(str::trim),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

/// `ANIE_PAGE_IN_BODIES`, parsed once per process.
fn env_page_in_bodies() -> bool {
    static BODIES: LazyLock<bool> = LazyLock::new(|| {
        resolve_page_in_bodies(std::env::var("ANIE_PAGE_IN_BODIES").ok().as_deref())
    });
    *BODIES
}

/// rlm2/PR4: sticky page-in state, session-scoped (owned by
/// the controller's `RlmSessionState`, shared with each
/// per-run policy) but keyed by the latest user prompt's
/// timestamp — so it resets exactly at run granularity (a
/// fresh prompt is a fresh run) while surviving the per-run
/// policy rebuilds of continuation runs, which share the
/// prompt and therefore the budget and the sticky set.
#[derive(Default)]
pub(crate) struct PageInRunState {
    /// Timestamp of the user prompt the sticky set and spend
    /// counter belong to. A fire that sees a different latest
    /// prompt timestamp resets the whole state.
    prompt_ts: Option<u64>,
    /// Rendered recall sections in page-in order. Append-only
    /// for the lifetime of a prompt, so re-rendering the
    /// consolidated recall message is byte-stable when no new
    /// item paged in — the no-op fast path depends on this.
    sticky_sections: Vec<String>,
    /// Timestamps of archive messages already recalled for
    /// this prompt. The same item is never paged in twice for
    /// one prompt.
    sticky_ts: HashSet<u64>,
    /// Estimated tokens spent on page-in this run, compared
    /// against the per-run budget.
    spent_tokens: u64,
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

    /// rlm2/PR3: when eviction fires (running total above the
    /// ceiling), evict down to `ceiling × evict_low_water_pct`
    /// in one batch rather than stopping at the ceiling, so
    /// the turns that follow are byte-stable appends. From
    /// `ANIE_EVICT_LOW_WATER_PCT`, clamped to [0.3, 0.9].
    evict_low_water_pct: f64,

    /// rlm2/PR5: token budget for the pinned tail — the
    /// longest trailing window of messages whose estimated
    /// total fits this budget is exempt from eviction.
    /// Replaces the positional `keep_last_n` (N messages):
    /// position is a terrible proxy for cost — 6 small
    /// assistant notes are ~100 tokens while 6 tool results
    /// can be 20k. Protects turn continuity; the trailing
    /// message is always pinned regardless of size, and the
    /// latest user + latest assistant messages are
    /// identity-pinned separately. If the pins together
    /// exceed the ceiling, the loop is over budget but the
    /// policy stops evicting (we'd rather be over ceiling
    /// than blind to the current turn). From
    /// `ANIE_PIN_TAIL_TOKENS` (default 3_072); the
    /// deprecated `ANIE_KEEP_LAST_N` converts at 512
    /// tokens/message.
    pin_tail_tokens: u64,

    /// Token budget for relevance-based paging-in (Phase E).
    /// Sits *on top* of `active_ceiling_tokens`: after
    /// FIFO eviction lands `working` at ≤ ceiling, the
    /// reranker may add up to this many tokens of
    /// keyword-relevant evicted content. Set to 0 to disable
    /// paging entirely (FIFO-only behavior, equivalent to
    /// pure Phase C).
    relevance_budget_tokens: u64,

    /// rlm2/PR4: cap on total page-in spend per run. The
    /// per-fire budget is `min(relevance_budget_tokens,
    /// run_budget - spent_so_far)`. From
    /// `ANIE_PAGE_IN_RUN_BUDGET`, default 8192.
    page_in_run_budget: u64,

    /// rlm2/PR4: `ANIE_PAGE_IN_BODIES` A/B hatch — `true`
    /// restores the pre-PR4 bodies-preferred selection.
    /// Default `false` (summaries-first).
    page_in_bodies: bool,

    /// rlm2/PR4: sticky page-in state. Session-scoped (shared
    /// with the controller's `RlmSessionState` via
    /// [`Self::with_page_in_state`]) but keyed by the latest
    /// user prompt's timestamp, so it self-resets per run.
    page_in_state: Arc<Mutex<PageInRunState>>,

    /// Shared with the recurse tool's
    /// `ControllerContextProvider` so evicted messages are
    /// readable via `RecurseScope::*`. The policy writes;
    /// the recurse tool reads.
    external: Arc<RwLock<ExternalContext>>,

    /// Timestamps of messages already pushed to `external`,
    /// for dedup. Shared with the controller's rlm session
    /// state (RC3, docs/code_review_2026-06-11.md): the
    /// store outlives any single run, so the dedup set must
    /// too — otherwise every new run's policy would re-
    /// archive (and re-enqueue summaries/embeds for) the
    /// messages the store already holds. Seeded once from
    /// the session-start snapshot.
    pushed: Arc<Mutex<HashSet<u64>>>,

    /// Timestamp of the ledger message injected on the
    /// previous fire, if any. Used to strip the stale ledger
    /// out of `request.context` before computing the new one
    /// so successive turns don't accumulate stale ledgers.
    /// `None` until the policy injects its first ledger.
    last_ledger_ts: Mutex<Option<u64>>,

    /// rlm2/PR4: timestamp of the consolidated archive-recall
    /// message injected on the previous fire, if any. Mirrors
    /// `last_ledger_ts` — the recall message is stripped from
    /// `request.context` alongside the ledger so it's never
    /// archived as conversational content and can be
    /// byte-compared against the rebuilt render for the no-op
    /// fast path.
    last_recall_ts: Mutex<Option<u64>>,

    /// rlm2/PR2: what the previous fire sent, pending comparison
    /// against the Ollama prefill count that comes back on the
    /// next assistant message. `None` until the first fire;
    /// consumed (`take`) at the start of every fire so a turn
    /// whose model call errored (no fresh assistant reply) can't
    /// false-alarm off a stale estimate.
    pending_truncation_check: Mutex<Option<TruncationCheck>>,

    /// rlm2/PR2: latched once the silent-truncation alarm has
    /// emitted its `SystemMessage`. The policy is per-run, so
    /// this is the "one-time-per-run" gate; the WARN log still
    /// fires on every detection.
    truncation_alarm_sent: AtomicBool,

    /// The effective `num_ctx` the run requests from Ollama, when
    /// known (`Some` only for native Ollama chat models; set by the
    /// controller via [`Self::with_ollama_num_ctx`]). The truncation
    /// detector needs it to tell a context shift (prefill near the
    /// window, send above it) from a healthy prefix-cache hit
    /// (prefill = the new suffix only); `None` keeps the alarm off.
    ollama_num_ctx: Option<u64>,

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

    /// Plan 04 §2d of `docs/local_model_augmentation/`:
    /// render the Small-tier "v2" ledger — plain
    /// `tool: "value"` lines with no `(id=...)` notation and
    /// a recurse instruction reduced to the single
    /// `message_grep` shape. The field session (notes F2)
    /// showed the v1 syntax manual leaking into bash
    /// commands on a 0.8B model. `false` (Full tier, or
    /// `ANIE_LEDGER=v1`) keeps the v1 ledger byte-identical.
    /// Wire schema is untouched: all recurse scopes still
    /// work, they're just not advertised.
    small_tier_ledger: bool,

    /// Cross-fire cache for the current prompt's embedding,
    /// keyed by the latest User message's timestamp. Filled
    /// by a background task (`spawn_prompt_embed_if_missing`)
    /// — never inline on the before_model path (RC2,
    /// docs/code_review_2026-06-11.md: the inline await here
    /// stalled the start of every model turn whenever Ollama
    /// was busy serving the live generation). Arc-shared so
    /// the spawned task can write the vector back after the
    /// fire has already returned.
    prompt_embed_cache: Arc<Mutex<PromptEmbedCache>>,
}

/// rlm2/PR2: the baseline the silent-truncation alarm compares the
/// next Ollama prefill count against.
#[derive(Debug, Clone, Copy)]
struct TruncationCheck {
    /// `estimate_tokens` of the working set the previous fire sent
    /// (survivors + paged-in + ledger) — the same value emitted as
    /// `RlmStatsUpdate::sent_context_tokens`.
    sent_context_tokens: u64,
    /// Timestamp of the newest assistant message at the time of
    /// that fire. The comparison only runs when the context's
    /// newest assistant message has a *different* timestamp — a
    /// fresh reply to the send we measured. Without this guard, a
    /// retried turn (model call errored, no new assistant message)
    /// would compare the new estimate against a stale reply's
    /// prefill count and could false-alarm.
    last_assistant_ts: Option<u64>,
}

/// State of the prompt-embedding cache slot.
#[derive(Default)]
enum PromptEmbedCache {
    /// Nothing cached (or the last attempt failed).
    #[default]
    Empty,
    /// A background task is embedding the prompt with this
    /// timestamp; don't spawn a duplicate.
    InFlight(u64),
    /// Embedding ready for the prompt with this timestamp.
    Ready(u64, Vec<f32>),
}

impl ContextVirtualizationPolicy {
    /// Build a policy bound to the given external store. The
    /// caller passes the shared set of timestamps already
    /// present in the store so we don't double-push them —
    /// session-scoped alongside the store itself (RC3), so a
    /// later run's policy sees everything earlier runs
    /// archived.
    pub(crate) fn new(
        active_ceiling_tokens: u64,
        pin_tail_tokens: u64,
        relevance_budget_tokens: u64,
        external: Arc<RwLock<ExternalContext>>,
        pushed: Arc<Mutex<HashSet<u64>>>,
    ) -> Self {
        Self {
            active_ceiling_tokens,
            evict_low_water_pct: env_evict_low_water_pct(),
            pin_tail_tokens,
            relevance_budget_tokens,
            page_in_run_budget: env_page_in_run_budget(),
            page_in_bodies: env_page_in_bodies(),
            page_in_state: Arc::new(Mutex::new(PageInRunState::default())),
            external,
            pushed,
            last_ledger_ts: Mutex::new(None),
            last_recall_ts: Mutex::new(None),
            pending_truncation_check: Mutex::new(None),
            truncation_alarm_sent: AtomicBool::new(false),
            ollama_num_ctx: None,
            event_tx: None,
            external_size: Arc::new(AtomicUsize::new(0)),
            summarizer_tx: None,
            embedder: None,
            embed_tx: None,
            small_tier_ledger: false,
            prompt_embed_cache: Arc::new(Mutex::new(PromptEmbedCache::Empty)),
        }
    }

    /// Pin the low-water fraction for deterministic tests
    /// (the constructor reads the process-wide env value,
    /// which a developer shell might override).
    #[cfg(test)]
    fn with_evict_low_water_pct(mut self, pct: f64) -> Self {
        self.evict_low_water_pct = pct;
        self
    }

    /// Record the effective Ollama context window (`num_ctx`) the
    /// run requests — `Some` only for native Ollama chat models.
    /// Without it the silent-truncation alarm stays off: a prefill
    /// undershoot can't be told apart from a prefix-cache hit.
    pub(crate) fn with_ollama_num_ctx(mut self, num_ctx: Option<u64>) -> Self {
        self.ollama_num_ctx = num_ctx;
        self
    }

    /// rlm2/PR4: share the session-scoped sticky page-in
    /// state. The controller passes `RlmSessionState`'s
    /// instance so continuation runs (fresh per-run policy,
    /// same prompt) keep the sticky set and the per-run spend
    /// counter; without this call the policy keeps a private
    /// instance (tests, and any future non-session install).
    pub(crate) fn with_page_in_state(mut self, state: Arc<Mutex<PageInRunState>>) -> Self {
        self.page_in_state = state;
        self
    }

    /// Pin the per-run page-in budget for deterministic tests
    /// (the constructor reads the process-wide env value).
    #[cfg(test)]
    fn with_page_in_run_budget(mut self, budget: u64) -> Self {
        self.page_in_run_budget = budget;
        self
    }

    /// Pin the bodies-preferred A/B hatch for deterministic
    /// tests (the constructor reads the process-wide env
    /// value).
    #[cfg(test)]
    fn with_page_in_bodies(mut self, enabled: bool) -> Self {
        self.page_in_bodies = enabled;
        self
    }

    /// Switch the per-turn ledger to the Small-tier v2 shape
    /// (plan 04 §2d). The controller sets this from the
    /// prompt tier + the `ANIE_LEDGER=v1` escape hatch.
    pub(crate) fn with_small_tier_ledger(mut self, enabled: bool) -> Self {
        self.small_tier_ledger = enabled;
        self
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

    /// rlm2/PR2: the silent-truncation alarm. Consumes the
    /// baseline stashed by the previous fire and compares it
    /// against the prefill count Ollama reported on the fresh
    /// assistant reply (`Usage::input_tokens` carries
    /// `prompt_eval_count` for the `ollama` provider). On a
    /// detector hit (`run_metrics::prefill_indicates_truncation`,
    /// the same predicate behind `context.truncation_suspected`:
    /// send above `num_ctx`, prefill near the window — a prefill
    /// far below it is a prefix-cache hit, never flagged): WARN on
    /// every detection, plus a one-time-per-run `SystemMessage`
    /// with the remediation hint. Hosted providers never match the
    /// provider gate, so non-Ollama behavior is unchanged; replies
    /// shaped by an error or abort never evaluated the send and
    /// are skipped.
    async fn alarm_on_silent_truncation(&self, context: &[Message]) {
        let pending = self
            .pending_truncation_check
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let Some(check) = pending else { return };
        let Some(reply) = latest_assistant(context) else {
            return;
        };
        // Only a reply that arrived *after* the measured send is
        // a valid prefill sample for it.
        if check.last_assistant_ts == Some(reply.timestamp) {
            return;
        }
        if reply.provider != crate::run_metrics::OLLAMA_PROVIDER {
            return;
        }
        // The agent loop synthesizes assistant messages for stream
        // failures and aborts (provider = the real provider, usage
        // defaulted) — those never evaluated the send, so their
        // usage is not a prefill sample.
        if !crate::run_metrics::is_completed_reply(reply) {
            return;
        }
        let prefilled = reply.usage.input_tokens;
        if !crate::run_metrics::prefill_indicates_truncation(
            check.sent_context_tokens,
            prefilled,
            self.ollama_num_ctx,
        ) {
            return;
        }
        warn!(
            target: "anie_cli::context_virt",
            sent_context_tokens = check.sent_context_tokens,
            prompt_eval_count = prefilled,
            "suspected silent truncation: Ollama evaluated far fewer tokens than were sent \
             (context-shift drops the OLDEST tokens, i.e. the system prompt)"
        );
        if self.truncation_alarm_sent.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(tx) = &self.event_tx {
            let _ = tx
                .send(AgentEvent::SystemMessage {
                    text: format_truncation_alarm(check.sent_context_tokens, prefilled),
                })
                .await;
        }
    }

    /// rlm2/PR2: stash this fire's sent-context estimate (and the
    /// newest assistant timestamp it was paired with) as the next
    /// fire's truncation baseline.
    fn record_truncation_baseline(&self, sent_context_tokens: u64, context: &[Message]) {
        *self
            .pending_truncation_check
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(TruncationCheck {
            sent_context_tokens,
            last_assistant_ts: latest_assistant(context).map(|a| a.timestamp),
        });
    }
}

/// Newest assistant message in the context, if any.
fn latest_assistant(context: &[Message]) -> Option<&AssistantMessage> {
    context.iter().rev().find_map(|m| match m {
        Message::Assistant(a) => Some(a),
        _ => None,
    })
}

/// rlm2/PR2: the one-time-per-run remediation hint for a suspected
/// silent truncation (the P1 bug class from the field notes:
/// ceiling == num_ctx, so Ollama context-shifted the system prompt
/// away with no visible signal).
fn format_truncation_alarm(sent_context_tokens: u64, prefill_tokens: u64) -> String {
    format!(
        "Ollama evaluated only {prefill_tokens} tokens of the ~{sent_context_tokens} sent — \
         the context was silently truncated (Ollama drops the oldest tokens, typically the \
         system prompt, when the window fills). Lower the active ceiling \
         (ANIE_ACTIVE_CEILING_TOKENS) or raise /context-length."
    )
}

/// rlm2/PR5: first index of the token-budgeted pinned tail.
/// Walk backward from the newest message accumulating
/// `estimate_tokens`; the tail is the longest contiguous
/// suffix whose total fits within `pin_tail_tokens`. The
/// trailing message is always pinned regardless of size —
/// evicting the tool result that *just* arrived would blind
/// the model to its own last action and trigger an immediate
/// re-call. Returns `working.len()` when nothing is pinned
/// (empty input only — the always-pin rule covers everything
/// else).
fn pinned_tail_start(working: &[Message], pin_tail_tokens: u64) -> usize {
    let mut remaining = pin_tail_tokens;
    let mut start = working.len();
    for (idx, m) in working.iter().enumerate().rev() {
        let cost = estimate_tokens(m);
        if idx + 1 == working.len() {
            start = idx;
            remaining = remaining.saturating_sub(cost);
            continue;
        }
        if cost > remaining {
            break;
        }
        remaining -= cost;
        start = idx;
    }
    start
}

/// rlm2 review fix: eviction must be pair-atomic. An assistant
/// message carrying `ToolCall` blocks and the `ToolResult`
/// messages answering them form one protocol unit — OpenAI-family
/// providers reject a context where either side survives without
/// the other (400: orphaned tool call / tool result). Maps each
/// index in `working` onto its pairing group: the assistant plus
/// every result answering one of its call ids. Indices with no
/// pair partner in `working` are absent (singleton units). Results
/// always follow their call in the transcript, so one forward pass
/// sees the owner before its results.
fn tool_pair_groups(working: &[Message]) -> HashMap<usize, Arc<Vec<usize>>> {
    let mut call_owner: HashMap<&str, usize> = HashMap::new();
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, m) in working.iter().enumerate() {
        match m {
            Message::Assistant(a) => {
                for block in &a.content {
                    if let ContentBlock::ToolCall(call) = block {
                        call_owner.insert(call.id.as_str(), idx);
                    }
                }
            }
            Message::ToolResult(t) => {
                if let Some(&owner) = call_owner.get(t.tool_call_id.as_str()) {
                    members
                        .entry(owner)
                        .or_insert_with(|| vec![owner])
                        .push(idx);
                }
            }
            _ => {}
        }
    }
    let mut groups: HashMap<usize, Arc<Vec<usize>>> = HashMap::new();
    for group in members.into_values() {
        let shared = Arc::new(group);
        for &idx in shared.iter() {
            groups.insert(idx, Arc::clone(&shared));
        }
    }
    groups
}

/// The anchors the eviction passes must not touch: the
/// token-budgeted tail plus the identity-pinned latest user and
/// latest assistant messages.
struct EvictionPins {
    tail_start: usize,
    user_ts: Option<u64>,
    assistant_ts: Option<u64>,
}

impl EvictionPins {
    fn is_pinned(&self, idx: usize, working: &[Message]) -> bool {
        if idx >= self.tail_start {
            return true;
        }
        match working.get(idx) {
            Some(Message::User(u)) => self.user_ts == Some(u.timestamp),
            Some(Message::Assistant(a)) => self.assistant_ts == Some(a.timestamp),
            _ => false,
        }
    }
}

/// Mark `idx` — together with its tool-call pair group, if any —
/// for eviction, charging every newly-marked member against
/// `running_total`. Returns the number of messages newly marked;
/// 0 when the unit is blocked because a member is pinned (evicting
/// around the pin would orphan one side of a tool_call/tool_result
/// pair). The pin check covering the whole group also makes a
/// pinned-tail snap unnecessary: a tool result inside the tail
/// keeps its (earlier) assistant call alive through the group.
fn evict_unit(
    idx: usize,
    working: &[Message],
    groups: &HashMap<usize, Arc<Vec<usize>>>,
    pins: &EvictionPins,
    to_evict: &mut Vec<usize>,
    running_total: &mut u64,
) -> usize {
    let singleton = [idx];
    let unit: &[usize] = groups.get(&idx).map_or(&singleton[..], |g| g.as_slice());
    if unit.iter().any(|&i| pins.is_pinned(i, working)) {
        return 0;
    }
    let mut marked = 0;
    for &i in unit {
        if to_evict.contains(&i) {
            continue;
        }
        let Some(m) = working.get(i) else { continue };
        *running_total = running_total.saturating_sub(estimate_tokens(m));
        to_evict.push(i);
        marked += 1;
    }
    marked
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

        // rlm2/PR2: before doing anything else, compare the
        // previous fire's sent-context estimate against the
        // prefill count Ollama reported on the reply — the
        // silent-truncation alarm.
        self.alarm_on_silent_truncation(request.context).await;

        // Step 1: strip the previous turn's ledger — and the
        // previous turn's archive-recall message (rlm2/PR4) —
        // out of working. If we left them in, archiving would
        // push policy metadata into `external` and the model
        // would see stale copies alongside the fresh ones.
        // rlm2/PR3: keep the stripped messages around — if
        // this turn is a pure append and the rebuilt ledger
        // (and rebuilt recall render) come out byte-identical,
        // we return `Continue` and the old messages stay
        // exactly where they are in the context. The recall
        // branch carries a content guard so a same-millisecond
        // timestamp collision with the ledger can't capture
        // the wrong message.
        let stale_ledger_ts = *self
            .last_ledger_ts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let stale_recall_ts = *self
            .last_recall_ts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut in_place_ledger: Option<&Message> = None;
        let mut in_place_recall: Option<&Message> = None;
        let mut working: Vec<Message> = Vec::with_capacity(request.context.len());
        for m in request.context {
            let ts = message_timestamp(m);
            if stale_recall_ts == Some(ts) && is_archive_recall(m) {
                in_place_recall = Some(m);
            } else if stale_ledger_ts == Some(ts) {
                in_place_ledger = Some(m);
            } else {
                working.push(m.clone());
            }
        }

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
        // Policy-injected reminders (the repo map, re-appended
        // with a fresh timestamp after the real prompt) are not
        // the directive — pinning one would leave the actual
        // prompt evictable (rlm2 review fix).
        let pinned_user_ts: Option<u64> = working.iter().rev().find_map(|m| match m {
            Message::User(u) if !is_reminder_user(u) => Some(u.timestamp),
            _ => None,
        });
        // rlm2/PR5: the latest assistant message is pinned
        // the same way. The token-budgeted tail can be
        // shorter than "the last user + assistant exchange"
        // when a giant tool result sits between them; the
        // identity pins guarantee the minimum the model
        // needs — its own latest reasoning plus the user's
        // directive — regardless of size.
        let pinned_assistant_ts: Option<u64> = working.iter().rev().find_map(|m| match m {
            Message::Assistant(a) => Some(a.timestamp),
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
        // rlm2/PR3 hysteresis: evicting to exactly the ceiling
        // guarantees the next append breaches it again — one
        // eviction (and one prefix-breaking ledger rebuild) per
        // turn. When the ceiling is breached, evict down to the
        // low-water mark in one batch so the following turns
        // append without evicting. Under the ceiling the target
        // equals the current total and the loops below are no-ops.
        let evict_target: u64 = if running_total > self.active_ceiling_tokens {
            (self.active_ceiling_tokens as f64 * self.evict_low_water_pct) as u64
        } else {
            running_total
        };
        // rlm2/PR5: the pinned tail is token-budgeted, not
        // positional — the longest trailing window that fits
        // `pin_tail_tokens`.
        let pins = EvictionPins {
            tail_start: pinned_tail_start(&working, self.pin_tail_tokens),
            user_ts: pinned_user_ts,
            assistant_ts: pinned_assistant_ts,
        };
        // rlm2 review fix: every pass below evicts whole
        // tool_call/tool_result groups via `evict_unit` — taking
        // one side without the other produces a context hosted
        // providers reject with a 400. A group with any pinned
        // member is skipped entirely.
        let eviction_groups = tool_pair_groups(&working);
        let mut to_evict: Vec<usize> = Vec::new();
        let mut supersedable_evicted: usize = 0;

        // 3a. Supersedable failures (Signal A).
        let supersedable = find_supersedable_failures(&working);
        for &idx in &supersedable {
            if running_total <= evict_target {
                break;
            }
            if to_evict.contains(&idx) {
                continue;
            }
            supersedable_evicted += evict_unit(
                idx,
                &working,
                &eviction_groups,
                &pins,
                &mut to_evict,
                &mut running_total,
            );
        }

        // 3a-bis. Stale failures (Signal B, position-based).
        // Failures that have aged past the still-fresh window
        // are evicted ahead of standard FIFO order — they're
        // the most likely-stale class of message in working.
        let mut stale_evicted: usize = 0;
        if running_total > evict_target {
            let stale = find_stale_failures(&working, pins.tail_start);
            for &idx in &stale {
                if running_total <= evict_target {
                    break;
                }
                if to_evict.contains(&idx) {
                    continue; // already evicted by Signal A
                }
                stale_evicted += evict_unit(
                    idx,
                    &working,
                    &eviction_groups,
                    &pins,
                    &mut to_evict,
                    &mut running_total,
                );
            }
        }

        // 3a-ter (rlm2/PR5): size-aware pass. Among the
        // evictable remainder, the oldest LARGE tool results
        // (> [`LARGE_TOOL_RESULT_EVICT_TOKENS`]) go before
        // standard FIFO — one stale file dump frees more
        // ceiling than dozens of small assistant texts, and
        // the small texts are what carry narrative
        // continuity at negligible cost.
        let mut large_evicted: usize = 0;
        if running_total > evict_target {
            for (idx, m) in working.iter().enumerate() {
                if running_total <= evict_target {
                    break;
                }
                if idx >= pins.tail_start {
                    break;
                }
                if to_evict.contains(&idx) {
                    continue;
                }
                if !matches!(m, Message::ToolResult(_)) {
                    continue;
                }
                if estimate_tokens(m) <= LARGE_TOOL_RESULT_EVICT_TOKENS {
                    continue;
                }
                large_evicted += evict_unit(
                    idx,
                    &working,
                    &eviction_groups,
                    &pins,
                    &mut to_evict,
                    &mut running_total,
                );
            }
        }

        // 3b. Standard FIFO eviction for the remainder. The
        // pinned user/assistant anchors are enforced inside
        // `evict_unit` (a unit containing one is skipped whole).
        for idx in 0..working.len() {
            if running_total <= evict_target {
                break;
            }
            // Skip pinned tail.
            if idx >= pins.tail_start {
                break;
            }
            // Already evicted by 3a / 3a-bis / 3a-ter.
            if to_evict.contains(&idx) {
                continue;
            }
            evict_unit(
                idx,
                &working,
                &eviction_groups,
                &pins,
                &mut to_evict,
                &mut running_total,
            );
        }
        // After 3a + 3a-ter + 3b, indices may not be sorted;
        // sort ascending so the reverse-drop below removes
        // the right items.
        to_evict.sort();
        to_evict.dedup();
        if supersedable_evicted > 0 || stale_evicted > 0 || large_evicted > 0 {
            debug!(
                supersedable_evicted,
                stale_evicted,
                large_evicted,
                fifo_evicted =
                    to_evict.len() - supersedable_evicted - stale_evicted - large_evicted,
                "rlm policy evicted failures (Signal A/B) + large tool results + FIFO"
            );
        }
        let evicted_count = to_evict.len();
        // rlm2/PR1: sum the estimated weight of what we're about to
        // drop, before the indices are invalidated by removal, so the
        // RunMetrics `context` block can attribute eviction by tokens
        // (the navigation tax is a token problem, not a count problem).
        let evicted_tokens: u64 = to_evict
            .iter()
            .filter_map(|&idx| working.get(idx))
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        if !to_evict.is_empty() {
            // Drop in reverse so earlier indices stay valid.
            for &idx in to_evict.iter().rev() {
                working.remove(idx);
            }
        }

        // Step 3.5 (rlm2/PR4): relevance-based recall. Score
        // every evicted message against the current prompt;
        // recall the highest scorers — summaries first — into
        // the consolidated archive-recall render, within
        // `min(relevance_budget_tokens, run budget remaining)`.
        // The recall budget overlays on top of the active
        // ceiling so total send is at most
        // `active_ceiling + relevance_budget`. Recalled
        // content never enters `working` itself: it renders
        // inside ONE `<system-reminder source="archive-recall">`
        // message appended right before the ledger below, so
        // the real transcript's chronology stays intact and
        // recalled items are structurally exempt from FIFO.
        let (recall_text, paged_in_count, paged_in_tokens) =
            self.recall_relevant_archive(&working).await;

        // Step 4: build the ledger from current external
        // state and append it. The ledger sits at the very
        // end of working, right before the model generates,
        // so it's maximally visible to the model.
        let ledger = self.build_ledger(&working).await;
        let ledger_ts = message_timestamp(&ledger);
        let ledger_tokens = estimate_tokens(&ledger);

        // rlm2/PR3: the append-only no-op fast path. When this
        // turn changed nothing about the working context's
        // shape — no eviction, no page-in — and the rebuilt
        // ledger is byte-identical to the one already sitting
        // in the context, replacing the messages would only
        // move identical ledger bytes from mid-context to the
        // end, invalidating Ollama's prefix cache from that
        // point for zero information gain. Return `Continue`
        // instead: the context goes out exactly as it arrived
        // (a pure extension of the previous turn's send), the
        // old ledger message stays in place, and
        // `last_ledger_ts` keeps pointing at it so a later
        // rebuilding turn still strips it. Store-side archiving
        // of new messages already happened in step 2 above —
        // the archive grows every turn regardless of which
        // path returns.
        let ledger_unchanged = match (
            in_place_ledger.and_then(first_text_of),
            first_text_of(&ledger),
        ) {
            (Some(old), Some(new)) => old == new,
            _ => false,
        };
        // rlm2/PR4: same byte-compare for the archive-recall
        // message. A turn whose only change would be
        // re-rendering an IDENTICAL recall (sticky sections,
        // no new page-ins) still qualifies as a no-op; a
        // recall appearing, disappearing (prompt changed,
        // sticky reset), or changing forces a rebuild.
        let recall_unchanged = match (
            in_place_recall.and_then(first_text_of),
            recall_text.as_deref(),
        ) {
            (None, None) => true,
            (Some(old), Some(new)) => old == new,
            _ => false,
        };
        if evicted_count == 0 && paged_in_count == 0 && ledger_unchanged && recall_unchanged {
            let sent_context_tokens: u64 = request
                .context
                .iter()
                .map(estimate_tokens)
                .fold(0u64, u64::saturating_add);
            self.record_truncation_baseline(sent_context_tokens, request.context);
            info!(
                target: "anie_cli::context_virt",
                archived_total,
                active_tokens = sent_context_tokens,
                ceiling = self.active_ceiling_tokens,
                "rlm policy fire (append-only no-op, ledger left in place)"
            );
            if let Some(tx) = &self.event_tx {
                let _ = tx
                    .send(AgentEvent::RlmStatsUpdate {
                        archived_messages: archived_total as u64,
                        evicted_count: 0,
                        evicted_tokens: 0,
                        paged_in_count: 0,
                        paged_in_tokens: 0,
                        ledger_tokens,
                        sent_context_tokens,
                    })
                    .await;
            }
            return BeforeModelResponse::Continue;
        }

        // rlm2/PR4: place ALL recalled content as one
        // consolidated message immediately before the ledger
        // (which stays strictly last — invariant b). Recorded
        // in `last_recall_ts` + `pushed` for the same reasons
        // as the ledger: stripped next fire, never archived.
        if let Some(text) = recall_text {
            let recall = Message::User(UserMessage {
                content: vec![ContentBlock::Text { text }],
                timestamp: now_millis(),
            });
            let recall_ts = message_timestamp(&recall);
            *self
                .last_recall_ts
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(recall_ts);
            self.pushed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(recall_ts);
            working.push(recall);
        } else {
            *self
                .last_recall_ts
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
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
        // rlm2/PR2: stash what we're about to send as the next
        // fire's truncation-alarm baseline (recorded whether or
        // not an event sender is attached — the WARN side of the
        // alarm doesn't need one).
        self.record_truncation_baseline(active_tokens, request.context);
        info!(
            target: "anie_cli::context_virt",
            archived_total,
            evicted = evicted_count,
            paged_in = paged_in_count,
            active_tokens,
            ceiling = self.active_ceiling_tokens,
            pin_tail_tokens = self.pin_tail_tokens,
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
                    evicted_count: evicted_count as u64,
                    evicted_tokens,
                    paged_in_count: paged_in_count as u64,
                    paged_in_tokens,
                    ledger_tokens,
                    // `active_tokens` is `estimate_tokens` of the full
                    // working set (survivors + paged-in + ledger) we're
                    // about to send — the truncation detector's
                    // baseline for the next turn's `prompt_eval_count`.
                    sent_context_tokens: active_tokens,
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

/// Maximum displayed identity entries per tool — the MOST
/// RECENT 8 (rlm2/PR5; earlier calls collapse into one
/// overflow line pointing at recurse message_grep). The
/// ledger has a soft 500-token target; even at 8 URL
/// strings × ~80 chars × 6 tool kinds we stay well under.
const TOOL_CALL_DISPLAY_CAP: usize = 8;

/// rlm2/PR5: the single overflow line appended after the
/// per-tool entries when the per-tool cap elided older
/// calls. One line total (not per tool) — the count is all
/// the model needs; the search instruction is the same
/// either way. `plain` selects the Small-tier no-syntax
/// phrasing (plan 04 §2d: notation leaks into small-model
/// tool calls).
fn format_elided_calls_line(elided: usize, plain: bool) -> String {
    let plural = if elided == 1 { "" } else { "s" };
    if plain {
        format!("{elided} earlier call{plural} not listed — search them with recurse message_grep.")
    } else {
        format!(
            "- {elided} earlier call{plural} not listed — search them with recurse \
             scope.kind=message_grep"
        )
    }
}

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
///
/// rlm2/PR5 (ledger diet): entries whose result bodies the
/// model can already see — timestamps in `visible_body_ts`,
/// i.e. still in the working set or recalled into the
/// sticky archive-recall message — are skipped. The ledger
/// exists to point at content reachable *only* via recurse;
/// repeating what's on screen is pure token tax. The skip
/// is deterministic for a given (archive, working, sticky)
/// state, so PR3's byte-stability holds: an entry reappears
/// exactly when its body is evicted or the sticky set
/// resets, both of which already force a ledger rebuild.
///
/// Ordering is deterministic and append-only (rlm2/PR3):
/// tools sort alphabetically; entries within a tool keep the
/// archive's insertion order (`ids_by_kind` preserves push
/// order), so unchanged history renders unchanged bytes and
/// new calls only append to existing lines.
fn collect_tool_call_summary(
    external: &ExternalContext,
    visible_body_ts: &HashSet<u64>,
) -> Vec<(String, Vec<ToolCallEntry>)> {
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
            // rlm2/PR5: result body already visible → skip.
            let body_ts = external
                .find_by_tool_call_id(&call.id)
                .and_then(|rid| external.get_by_id(rid))
                .map(message_timestamp);
            if body_ts.is_some_and(|ts| visible_body_ts.contains(&ts)) {
                continue;
            }
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
/// like `- web_read targets: foo (id=ollama_tc_8_2),
/// bar (id=ollama_tc_8_3)`. Each entry surfaces the
/// `tool_call_id` so `RecurseScope::ToolResult` is
/// directly usable. Empty input yields no lines so the
/// ledger stays compact when nothing has been called yet.
/// rlm2/PR5: each tool shows its MOST RECENT
/// [`TOOL_CALL_DISPLAY_CAP`] entries; all elided older
/// calls collapse into one trailing overflow line naming
/// recurse message_grep.
fn render_tool_call_summary_lines(summary: &[(String, Vec<ToolCallEntry>)]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut elided_total = 0usize;
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
        elided_total += total - display;
        // Format: `<value> (id=<tool_call_id>)`. Value
        // first because that's what the model needs to match
        // against the user's question. The `(id=...)` suffix
        // is the recurse-tool reference. Earlier `[id=X] Y`
        // was misread as "the value is `[id=X]`" — qwen3.5:9b
        // was passing the bracketed string as a tool_call_id.
        let rendered: Vec<String> = args
            .iter()
            .skip(total - display)
            .map(|e| format!("{} (id={})", e.arg_value, e.tool_call_id))
            .collect();
        lines.push(format!("- {tool_name} {label}: {}", rendered.join(", ")));
    }
    if elided_total > 0 {
        lines.push(format_elided_calls_line(elided_total, false));
    }
    lines
}

/// Plan 04 §2d: render `(tool_name, entries)` pairs as plain
/// Small-tier ledger lines — `web_search: "query one", "query
/// two"`. Deliberately NO `(id=...)` suffix: the v1 notation
/// was pattern-matched into bash commands by the field-session
/// model (notes F2). Same display cap as the v1 renderer
/// (rlm2/PR5: most recent entries, one overflow line).
fn render_plain_tool_call_lines(summary: &[(String, Vec<ToolCallEntry>)]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut elided_total = 0usize;
    for (tool_name, args) in summary {
        if args.is_empty() {
            continue;
        }
        let total = args.len();
        let display = total.min(TOOL_CALL_DISPLAY_CAP);
        elided_total += total - display;
        let rendered: Vec<String> = args
            .iter()
            .skip(total - display)
            .map(|e| format!("\"{}\"", e.arg_value))
            .collect();
        lines.push(format!("{tool_name}: {}", rendered.join(", ")));
    }
    if elided_total > 0 {
        lines.push(format_elided_calls_line(elided_total, true));
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

/// rlm2/PR4: one recall section for an archive entry whose
/// summary is being paged in. Names the entry id so the model
/// can fetch the full body via recurse.
fn render_summary_section(id: crate::external_context::MessageId, summary: &str) -> String {
    format!("[archive entry {id} — summary; full body via recurse]\n{summary}")
}

/// rlm2/PR4: one recall section for a small, unsummarized
/// archive entry whose full body is being paged in.
fn render_body_section(id: crate::external_context::MessageId, body: &str) -> String {
    format!("[archive entry {id}]\n{body}")
}

/// rlm2/PR4: render the consolidated archive-recall message
/// text from the sticky sections. Deterministic in section
/// order, so a turn with no new page-ins re-renders the exact
/// bytes of the previous turn's recall message — the no-op
/// fast path byte-compares this output.
fn render_archive_recall(sections: &[String]) -> String {
    let mut lines = vec![
        ARCHIVE_RECALL_OPEN.to_string(),
        "Relevant archived content recalled for the current prompt. Summaries are lossy — \
         fetch any full body with the recurse tool."
            .to_string(),
    ];
    for section in sections {
        lines.push(String::new());
        lines.push(section.clone());
    }
    lines.push("</system-reminder>".to_string());
    lines.join("\n")
}

/// rlm2/PR4: token cost of one recall section — the same
/// `estimate_tokens` lens the eviction accounting uses,
/// applied to the section text as it will actually render.
fn section_tokens(text: &str) -> u64 {
    estimate_tokens(&Message::User(UserMessage {
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        timestamp: 0,
    }))
}

impl ContextVirtualizationPolicy {
    /// Non-blocking read of the prompt-embedding cache.
    /// Returns the vector only when the background task has
    /// finished embedding the prompt with this timestamp.
    fn peek_prompt_embedding(&self, ts: u64) -> Option<Vec<f32>> {
        let slot = self
            .prompt_embed_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        match &*slot {
            PromptEmbedCache::Ready(cached_ts, vec) if *cached_ts == ts => Some(vec.clone()),
            _ => None,
        }
    }

    /// Kick off a background task that embeds the current
    /// prompt and writes the vector into the shared cache
    /// slot. No-op when no embedder is configured or when
    /// the slot already holds (or is computing) this
    /// prompt's embedding. Never awaited by the caller —
    /// the current fire reranks by keyword overlap and the
    /// next fire picks the cached vector up (RC2,
    /// docs/code_review_2026-06-11.md: awaiting the embed
    /// inline blocked the start of every model turn behind
    /// the same Ollama instance serving the live
    /// generation).
    fn spawn_prompt_embed_if_missing(&self, text: String, ts: u64) {
        let Some(embedder) = self.embedder.as_ref().map(Arc::clone) else {
            return;
        };
        {
            let mut slot = self
                .prompt_embed_cache
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match &*slot {
                PromptEmbedCache::InFlight(t) | PromptEmbedCache::Ready(t, _) if *t == ts => {
                    return;
                }
                _ => {}
            }
            *slot = PromptEmbedCache::InFlight(ts);
        }
        let cache = Arc::clone(&self.prompt_embed_cache);
        tokio::spawn(async move {
            let result = embedder.embed(&text).await;
            let mut slot = cache.lock().unwrap_or_else(|p| p.into_inner());
            // Only write back if a newer prompt hasn't
            // claimed the slot in the meantime.
            if !matches!(&*slot, PromptEmbedCache::InFlight(t) if *t == ts) {
                return;
            }
            match result {
                Ok(vec) => *slot = PromptEmbedCache::Ready(ts, vec),
                Err(error) => {
                    tracing::warn!(
                        target: "anie_cli::context_virt",
                        %error,
                        "background prompt embed failed; reranker stays on keyword overlap"
                    );
                    *slot = PromptEmbedCache::Empty;
                }
            }
        });
    }

    /// rlm2/PR4: score evicted messages against the current
    /// prompt's keywords (or cached embedding) and recall the
    /// highest scorers into the consolidated archive-recall
    /// render — summaries first; a full body only when no
    /// summary exists AND the body is under
    /// [`PAGE_IN_BODY_MAX_TOKENS`] (`ANIE_PAGE_IN_BODIES=1`
    /// restores the old bodies-preferred selection). The
    /// per-fire budget is `min(relevance_budget_tokens,
    /// per-run budget remaining)`.
    ///
    /// Returns `(recall_text, newly_paged_count,
    /// newly_paged_tokens)`. `recall_text` is the FULL render
    /// — every sticky section recalled so far for the current
    /// prompt, not just this fire's additions — so an item
    /// stays visible until the prompt changes. The counters
    /// cover only this fire's additions: rlm2/PR1's
    /// `paged_in_tokens` keeps measuring new page-in spend,
    /// and a fire that merely re-renders identical sticky
    /// content reports zeros (and can no-op).
    async fn recall_relevant_archive(&self, working: &[Message]) -> (Option<String>, usize, u64) {
        if self.relevance_budget_tokens == 0 {
            return (None, 0, 0);
        }
        let prompt_text_ts = latest_user_prompt(working);
        let Some((_, prompt_ts)) = prompt_text_ts.as_ref() else {
            return (None, 0, 0);
        };
        // Sticky reset: the state belongs to one prompt. A
        // different latest-prompt timestamp means a new run —
        // clear the sticky set AND the per-run spend counter.
        {
            let mut state = self.page_in_state.lock().unwrap_or_else(|p| p.into_inner());
            if state.prompt_ts != Some(*prompt_ts) {
                *state = PageInRunState {
                    prompt_ts: Some(*prompt_ts),
                    ..PageInRunState::default()
                };
            }
        }
        // Snapshot what's already recalled + how much budget
        // is left, without holding the lock across the store
        // read below.
        let (sticky_ts, mut budget) = {
            let state = self.page_in_state.lock().unwrap_or_else(|p| p.into_inner());
            let run_remaining = self.page_in_run_budget.saturating_sub(state.spent_tokens);
            (
                state.sticky_ts.clone(),
                self.relevance_budget_tokens.min(run_remaining),
            )
        };
        let Some(prompt_tokens) = current_prompt_tokens(working) else {
            return (self.render_sticky_recall(), 0, 0);
        };
        if budget == 0 {
            return (self.render_sticky_recall(), 0, 0);
        }

        // Plan-08: when an embedder is configured and the
        // background task has already embedded the current
        // prompt, score candidates by cosine similarity.
        // On a cache miss this fire falls back to keyword
        // overlap; the embed is kicked off below — after we
        // know the archive actually holds something to
        // rerank — so the turn path never waits on it.
        let prompt_embed = prompt_text_ts
            .as_ref()
            .and_then(|(_, ts)| self.peek_prompt_embedding(*ts));

        // Score and select while holding the read guard.
        // rlm2/PR5 (perf): candidates BORROW from the store —
        // scoring intersects the prompt's tokens with each
        // entry's token set cached at archive time, and the
        // budget loop renders owned section text only for
        // the items it accepts. Unselected bodies are never
        // cloned (enforced by `RelevanceCandidate<'_>`'s
        // lifetime). No `.await` runs while the guard is
        // held, so the future stays Send.
        let working_ts: HashSet<u64> = working.iter().map(message_timestamp).collect();
        let mut candidate_pool = 0usize;
        let accepted: Vec<(u64, String, u64)> = {
            let external = self.external.read().await;
            let mut candidates: Vec<RelevanceCandidate<'_>> = external
                .iter_stored()
                .filter(|s| {
                    let ts = message_timestamp(&s.message);
                    !working_ts.contains(&ts) && !sticky_ts.contains(&ts)
                })
                .inspect(|_| candidate_pool += 1)
                .filter_map(|s| {
                    let score = score_candidate(prompt_embed.as_deref(), &prompt_tokens, s);
                    if score <= 0.0 {
                        None
                    } else {
                        Some(RelevanceCandidate {
                            score,
                            id: s.id,
                            message: &s.message,
                            summary: s.summary.as_deref(),
                        })
                    }
                })
                .collect();

            // Sort by score descending; tie-break by recency
            // (later timestamps preferred). NaN guard: if a
            // score ever ends up NaN (shouldn't with our
            // cosine guards), treat it as Equal so the sort
            // stays total.
            candidates.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| message_timestamp(b.message).cmp(&message_timestamp(a.message)))
            });

            // Budget loop: pick the section form for each
            // candidate, charge its rendered cost against the
            // per-fire budget, and collect the accepted
            // sections (the only owned data this pass makes).
            let mut accepted: Vec<(u64, String, u64)> = Vec::new();
            for candidate in candidates {
                let RelevanceCandidate {
                    id,
                    message,
                    summary,
                    ..
                } = candidate;
                let ts = message_timestamp(message);
                let body_text = first_text_of(message);
                let section = if self.page_in_bodies {
                    // A/B hatch: the pre-PR4 preference — full
                    // body when it fits the budget, summary as
                    // the fallback.
                    let body_section = body_text.map(|t| render_body_section(id, t));
                    match body_section {
                        Some(s) if section_tokens(&s) <= budget => Some(s),
                        _ => summary.map(|s| render_summary_section(id, s)),
                    }
                } else if let Some(summary_text) = summary {
                    // Summaries-first: the ledger already tells
                    // the model `recurse` fetches the full body.
                    Some(render_summary_section(id, summary_text))
                } else if estimate_tokens(message) < PAGE_IN_BODY_MAX_TOKENS {
                    body_text.map(|t| render_body_section(id, t))
                } else {
                    // Large unsummarized body: reachable via
                    // recurse only.
                    None
                };
                let Some(section) = section else { continue };
                let cost = section_tokens(&section);
                if cost > budget {
                    continue;
                }
                budget = budget.saturating_sub(cost);
                accepted.push((ts, section, cost));
                if budget == 0 {
                    break;
                }
            }
            accepted
        };

        // Prompt-embed cache miss with a live candidate
        // pool: compute the embedding off the turn path so
        // the NEXT fire can rerank semantically. Skipped
        // entirely when the archive holds nothing to rerank
        // (RC2: the embed used to fire before the empty-
        // candidates check, paying an HTTP call for turns
        // with no archive at all).
        if candidate_pool > 0 && prompt_embed.is_none() {
            if let Some((text, ts)) = prompt_text_ts {
                self.spawn_prompt_embed_if_missing(text, ts);
            }
        }

        // Fold the accepted sections into the sticky state
        // and render the full recall (old sticky sections +
        // this fire's additions). Counters cover only the
        // additions.
        let mut state = self.page_in_state.lock().unwrap_or_else(|p| p.into_inner());
        let mut paged = 0usize;
        let mut paged_tokens = 0u64;
        for (ts, section, cost) in accepted {
            if !state.sticky_ts.insert(ts) {
                continue;
            }
            state.sticky_sections.push(section);
            state.spent_tokens = state.spent_tokens.saturating_add(cost);
            paged += 1;
            paged_tokens = paged_tokens.saturating_add(cost);
        }
        let recall = if state.sticky_sections.is_empty() {
            None
        } else {
            Some(render_archive_recall(&state.sticky_sections))
        };
        (recall, paged, paged_tokens)
    }

    /// rlm2/PR4: render the current sticky sections (no new
    /// additions). The early-return paths of
    /// [`Self::recall_relevant_archive`] use this so an
    /// already-recalled item stays visible even on fires that
    /// can't score new candidates (budget exhausted, empty
    /// candidate pool, prompt with no scorable text).
    fn render_sticky_recall(&self) -> Option<String> {
        let state = self.page_in_state.lock().unwrap_or_else(|p| p.into_inner());
        if state.sticky_sections.is_empty() {
            None
        } else {
            Some(render_archive_recall(&state.sticky_sections))
        }
    }

    /// rlm2/PR5: timestamps of every message body the model
    /// can already see this turn — the working set plus the
    /// sticky archive-recall set. Ledger identity entries
    /// whose result bodies are in this set are skipped
    /// (diet): the ledger points at content reachable only
    /// via recurse.
    fn visible_body_ts(&self, working: &[Message]) -> HashSet<u64> {
        let mut ts: HashSet<u64> = working.iter().map(message_timestamp).collect();
        let state = self.page_in_state.lock().unwrap_or_else(|p| p.into_inner());
        ts.extend(state.sticky_ts.iter().copied());
        ts
    }

    /// Plan 04 §2d: the Small-tier ledger. Plain
    /// `tool: "value"` lines — no ids, no scope grammar —
    /// because the only syntax a small model can use
    /// correctly is no syntax at all. Exactly one recurse
    /// shape is advertised (`message_grep`, the one the
    /// field session showed the model reaching for); the
    /// other scopes stay live on the wire, just unlisted.
    async fn build_small_tier_ledger_lines(&self, working: &[Message]) -> Vec<String> {
        let visible_body_ts = self.visible_body_ts(working);
        let external = self.external.read().await;
        // rlm2/PR3: count only evicted messages, for the same
        // byte-stability reason as the v1 header — totals grow
        // on every append and would defeat the no-op fast path.
        let evicted = external.len().saturating_sub(working.len());
        let mut lines = vec![
            "<system-reminder>".to_string(),
            format!("Archive: {evicted} older messages are saved outside this conversation."),
        ];
        let summary = collect_tool_call_summary(&external, &visible_body_ts);
        let call_lines = render_plain_tool_call_lines(&summary);
        if !call_lines.is_empty() {
            lines.push("These tool calls were already made — do NOT repeat them:".to_string());
            lines.extend(call_lines);
        }
        lines.push(
            "To search the archived results, call recurse with arguments {\"scope\": {\"kind\": \"message_grep\", \"pattern\": \"<words>\"}}.".to_string(),
        );
        lines.push("</system-reminder>".to_string());
        lines
    }

    /// Build the structured ledger as a `User` message
    /// wrapped in `<system-reminder>` tags. Counts come from
    /// the shared `ExternalContext` indexes; tool-result
    /// breakdown is sorted by frequency and capped at 8 names
    /// to keep the ledger bounded (target ≤500 tokens).
    /// rlm2/PR4: the ledger no longer reports a per-turn
    /// paged-in count — recalled content is self-describing
    /// in the adjacent archive-recall message, and dropping
    /// the count keeps the ledger bytes stable across the
    /// turn that follows a page-in (fast-path discipline).
    async fn build_ledger(&self, working: &[Message]) -> Message {
        if self.small_tier_ledger {
            let lines = self.build_small_tier_ledger_lines(working).await;
            return Message::User(UserMessage {
                content: vec![ContentBlock::Text {
                    text: lines.join("\n"),
                }],
                timestamp: now_millis(),
            });
        }
        let visible_body_ts = self.visible_body_ts(working);
        let lines = {
            let external = self.external.read().await;
            let total = external.len();
            let evicted = total.saturating_sub(working.len());

            // Imperative header. Earlier versions said "use
            // the recurse tool to access evicted content" —
            // permissive language the model treated as
            // optional, leading to repeated re-fetches of
            // URLs already in the archive. The directive
            // form below is explicit: scan the lists, prefer
            // recurse over re-running tools whose targets
            // are already listed.
            // rlm2/PR3: the header counts only EVICTED
            // messages. The total/active counts grow with
            // every appended message, which would change the
            // ledger bytes every turn and starve the
            // append-only fast path; the evicted count is
            // stable across appends (new messages land in
            // both the archive and the working set) and is
            // the number the model actually needs — how much
            // is reachable only via recurse.
            let mut lines = vec![
                "<system-reminder>".to_string(),
                format!("external context — {evicted} evicted messages in the archive"),
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
            let summary = collect_tool_call_summary(&external, &visible_body_ts);
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

/// PR 4 Signal B (simplified) of `docs/harness_mitigations_2026-05-01/`.
/// Walk `working` and identify failed tool results that
/// have aged past a "still-fresh" window. A failure is
/// considered stale when:
///
/// - It's a `Message::ToolResult` with `is_error == true`,
/// - It's NOT in the pinned tail (`idx < pinned_tail_start`;
///   still pinned; would have been skipped anyway),
/// - At least `min_messages_after = 4` messages follow it
///   in `working` — heuristic for "≥ 2 turns dwell" without
///   needing turn-tracking metadata.
///
/// Returns indices into `working`, sorted ascending.
///
/// **Why simplified vs. the original plan:** the plan
/// called for embedding-based bottom-quartile relevance
/// scoring against the current prompt. That requires
/// embedding all active-context messages on every fire and
/// passing the embedder + prompt embedding through to the
/// eviction logic. This iteration ships a position-based
/// approximation that addresses the core observation
/// (older failures are usually no longer relevant). True
/// embedding-based ranking can land as a v2 if smoke shows
/// position alone isn't sharp enough.
fn find_stale_failures(working: &[Message], pinned_tail_start: usize) -> Vec<usize> {
    const MIN_MESSAGES_AFTER: usize = 4;
    let working_len = working.len();
    let mut stale = Vec::new();
    for (idx, m) in working.iter().enumerate() {
        if let Message::ToolResult(tr) = m
            && tr.is_error
            && idx < pinned_tail_start
            && working_len.saturating_sub(idx) > MIN_MESSAGES_AFTER
        {
            stale.push(idx);
        }
    }
    stale
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
            run_usage: Box::leak(Box::new(anie_protocol::Usage::default())),
        }
    }

    /// Wrap a pushed-timestamps set in the session-shared
    /// shape the policy constructor takes.
    fn shared_pushed(set: HashSet<u64>) -> Arc<Mutex<HashSet<u64>>> {
        Arc::new(Mutex::new(set))
    }

    /// With `u64::MAX` ceiling the policy never evicts —
    /// `Continue` on every call. This is the default-install
    /// behavior the controller falls through to when the
    /// operator hasn't opted into a ceiling.
    #[tokio::test]
    async fn ceiling_unlimited_returns_continue() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let policy =
            ContextVirtualizationPolicy::new(u64::MAX, 4, 0, store, shared_pushed(HashSet::new()));
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
        let policy =
            ContextVirtualizationPolicy::new(10_000, 4, 0, store, shared_pushed(HashSet::new()));
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

    /// Over ceiling: evicts oldest first, pins the trailing
    /// token-budgeted tail. With 10 one-token messages,
    /// ceiling tight enough to require eviction, and
    /// `pin_tail_tokens = 3`, the result keeps the last 3
    /// at minimum and evicts older ones from the front.
    #[tokio::test]
    async fn over_ceiling_evicts_oldest_keeps_pinned_tail() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // Each user("..", ts) message is roughly 1 token of
        // text content ("msgN") plus overhead. With a tiny
        // ceiling we force eviction.
        let context: Vec<Message> = (0..10)
            .map(|i| user(&format!("msg{i}"), i as u64))
            .collect();
        // Ceiling = 5 tokens; pin_tail_tokens = 3 → pins the
        // last three 1-token messages.
        let policy =
            ContextVirtualizationPolicy::new(5, 3, 0, store, shared_pushed(HashSet::new()));
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
        let policy =
            ContextVirtualizationPolicy::new(2, 2, 0, store, shared_pushed(HashSet::new()));
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

    /// rlm2 review fix: the repo-map policy appends its
    /// `<system-reminder source="repo-map">` message AFTER the real
    /// prompt with a fresh timestamp, making it the newest User
    /// message at every fire. The latest-user pin must skip it —
    /// pinning the reminder leaves the actual directive evictable,
    /// exactly the rlm/17 confabulation regression the pin exists
    /// to prevent.
    #[tokio::test]
    async fn latest_user_pin_skips_policy_reminder_messages() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let reminder = "<system-reminder source=\"repo-map\">\nsrc/lib.rs\n</system-reminder>";
        let context = vec![
            user("Just say done.", 1), // the directive — must survive
            assistant("ok let me read", 2),
            tool_result("c1", "read", "lots of file content here", 3),
            assistant("read result", 4),
            tool_result("c2", "read", "more file content", 5),
            user(reminder, 6), // policy-injected, newest user message
        ];
        let policy =
            ContextVirtualizationPolicy::new(2, 2, 0, store, shared_pushed(HashSet::new()));
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let has_user_directive = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text == "Just say done.",
                _ => false,
            },
            _ => false,
        });
        assert!(
            has_user_directive,
            "the directive, not the repo-map reminder, is the pinned user message: {survivors:?}"
        );
    }

    /// rlm2 review fix: prompt identification (the reranker's
    /// keyword source, the embed/sticky key) reads the latest REAL
    /// user message, skipping policy-injected reminders.
    #[test]
    fn prompt_identification_skips_reminder_user_messages() {
        let working = vec![
            user("what is the weather in tallahassee", 1),
            user(
                "<system-reminder source=\"repo-map\">\nsrc/repo_map.rs\n</system-reminder>",
                2,
            ),
        ];
        let (text, ts) = latest_user_prompt(&working).expect("the real prompt is found");
        assert_eq!(ts, 1, "keyed to the directive, not the reminder");
        assert!(text.contains("tallahassee"), "{text}");
        let toks = current_prompt_tokens(&working).expect("tokens");
        assert!(toks.contains("tallahassee"), "{toks:?}");
        assert!(
            !toks.contains("repo"),
            "repo-map file paths must not drive the reranker: {toks:?}"
        );
    }

    /// Pinned tail itself exceeds the ceiling: the policy
    /// keeps the tail anyway and stops evicting. We'd rather
    /// be over budget than blind to the current turn.
    #[tokio::test]
    async fn pinned_tail_overrides_ceiling() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..6).map(|i| user(&format!("msg{i}"), i as u64)).collect();
        // Ceiling = 1 token (impossibly tight);
        // pin_tail_tokens = 5 pins the last five 1-token
        // messages. The pinned tail will be over the ceiling
        // but the policy refuses to evict pinned messages.
        let policy =
            ContextVirtualizationPolicy::new(1, 5, 0, store, shared_pushed(HashSet::new()));
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
        let policy = ContextVirtualizationPolicy::new(
            5,
            2,
            0,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
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
        let policy =
            ContextVirtualizationPolicy::new(5, 2, 0, Arc::clone(&external), shared_pushed(pushed));
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
        let policy = ContextVirtualizationPolicy::new(
            5,
            2,
            0,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
        let response = policy.before_model(sample_request(&context)).await;

        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Pinned tail (last 2 originals) + ledger at the
        // end. pin_tail_tokens=2 fits assistant("ack2") and
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

    /// Ledger does not accumulate across fires: a turn that
    /// changes the ledger content strips the previous turn's
    /// ledger before injecting the fresh one. After two
    /// rebuilding fires the survivors contain exactly one
    /// ledger, not two. (rlm2/PR3: a turn whose rebuilt
    /// ledger would be byte-identical takes the `Continue`
    /// fast path instead — `ledger_bytes_stable_across_appending_turns`
    /// covers that side; here the appended tool result
    /// changes the tool-result breakdown, forcing a rebuild.)
    #[tokio::test]
    async fn ledger_replaced_each_turn_no_accumulation() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..3).map(|i| user(&format!("msg{i}"), i as u64)).collect();
        let policy =
            ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(HashSet::new()));

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
        // ReplaceMessages output as the new state) plus a new
        // tool result, which changes the ledger's tool-result
        // breakdown and forces a rebuild.
        let mut context_2 = after_fire_1;
        context_2.push(tool_result("c1", "bash", "ls output", 50));
        let r2 = policy.before_model(sample_request(&context_2)).await;
        let after_fire_2 = match r2 {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        // Exactly one ledger; the old one was stripped and the
        // fresh one is strictly last.
        assert_eq!(after_fire_2.len(), 5);
        assert_eq!(after_fire_2.iter().filter(|m| is_ledger(m)).count(), 1);
        assert!(is_ledger(after_fire_2.last().expect("non-empty")));
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
        let policy =
            ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(HashSet::new()));
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
        // rlm2/PR3: the header counts evicted messages only
        // (nothing evicted here — everything is active).
        assert!(ledger_text.contains("0 evicted messages in the archive"));
        assert!(ledger_text.contains("3 tool results"));
        assert!(ledger_text.contains("bash x2"));
        assert!(ledger_text.contains("read x1"));
    }

    /// Plan 04 §2d: the Small-tier ledger lists prior calls
    /// as plain `tool: "value"` lines — the `(id=...)`
    /// notation that the field session showed leaking into
    /// bash commands (notes F2) never appears.
    #[tokio::test]
    async fn small_tier_ledger_contains_no_id_notation() {
        // The call's result body lives only in the archive
        // (rlm2/PR5: entries whose bodies are still in the
        // working set are skipped as redundant).
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            user("what's the time?", 1),
            assistant_with_tool_call(
                "web_search",
                serde_json::json!({"query": "rust testing"}),
                2,
            ),
            tool_result("call_2", "web_search", "result body", 3),
        ])));
        let pushed: HashSet<u64> = (1..=3).collect();
        let context = vec![user("and in Tokyo?", 200)];
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(pushed))
            .with_small_tier_ledger(true);
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let text = user_text(survivors.last().expect("non-empty")).expect("ledger text");
        assert!(text.contains("web_search: \"rust testing\""), "{text}");
        assert!(text.contains("do NOT repeat"), "{text}");
        assert!(!text.contains("(id="), "{text}");
        assert!(
            !text.contains("call_2"),
            "no tool-call ids anywhere: {text}"
        );
    }

    /// Plan 04 §2d: the Small-tier recurse instruction is
    /// exactly one JSON shape (`message_grep`); the other
    /// scopes stay usable on the wire but are not advertised,
    /// and the v1 `scope.kind=` grammar is gone.
    #[tokio::test]
    async fn small_tier_recurse_instruction_advertises_only_message_grep() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context = vec![user("hi", 1), assistant("hello", 2)];
        let policy =
            ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(HashSet::new()))
                .with_small_tier_ledger(true);
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let text = user_text(survivors.last().expect("non-empty")).expect("ledger text");
        assert!(
            text.contains("{\"scope\": {\"kind\": \"message_grep\", \"pattern\": \"<words>\"}}"),
            "{text}"
        );
        assert!(!text.contains("scope.kind="), "{text}");
        assert!(!text.contains("tool_result"), "{text}");
        assert!(!text.contains("tool_call_id"), "{text}");
        assert!(!text.contains("archive_id"), "{text}");
    }

    /// Plan 04 §2d regression guard: with the Small-tier flag
    /// off (Full tier, or `ANIE_LEDGER=v1`), the ledger is
    /// byte-identical to this v1 fixture — drift fails the
    /// byte compare. (Fixture updated by rlm2/PR3: the header
    /// counts evicted messages only; and by rlm2/PR5: the
    /// archived exchange lives outside the working set, since
    /// identity entries whose bodies are in the working set
    /// are now skipped.)
    #[tokio::test]
    async fn full_tier_ledger_unchanged() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            user("u0", 1),
            assistant_with_tool_call(
                "web_search",
                serde_json::json!({"query": "rust testing"}),
                2,
            ),
            tool_result("call_2", "web_search", "result body", 3),
        ])));
        let pushed: HashSet<u64> = (1..=3).collect();
        let context = vec![user("follow-up question", 200)];
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(pushed));
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let text = user_text(survivors.last().expect("non-empty")).expect("ledger text");
        let expected = "<system-reminder>\n\
            external context — 3 evicted messages in the archive\n\
            \n\
            Before issuing a new tool call, scan the lists below.\n\
            If the URL, query, command, or path you're about to use is already listed,\n\
            the result is in the archive — do NOT re-run the tool. Use `recurse` instead:\n\
            \x20 - `scope.kind=message_grep`, `pattern=<regex>` — search archived messages\n\
            \x20   by keyword. Easiest option; needs no id.\n\
            \x20 - `scope.kind=tool_result`, `tool_call_id=<id>` — fetch one prior result\n\
            \x20   verbatim. Each ledger entry is `<value> (id=<call_id>)`; pass the\n\
            \x20   `<call_id>` (without the surrounding parens) as the tool_call_id.\n\
            \x20 - `scope.kind=summary`, `id=<archive_id>` — fetch the gist. Cheapest.\n\
            Re-running a tool whose output is already archived wastes user time.\n\
            \n\
            - 1 tool results: web_search x1\n\
            - web_search queries: rust testing (id=call_2)\n\
            </system-reminder>";
        assert_eq!(text, expected);
    }

    /// Ledger is not archived to `external` — the recurse
    /// tool surfaces conversational content, not policy
    /// metadata. After a fire that injects a ledger, the
    /// store size matches the input count exactly.
    #[tokio::test]
    async fn ledger_not_archived_to_external() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context: Vec<Message> = (0..3).map(|i| user(&format!("msg{i}"), i as u64)).collect();
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            0,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
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

    /// rlm2/PR5: keyword scoring is the intersection size
    /// between the prompt's tokens and the candidate's token
    /// set cached at archive time. Stopwords don't count.
    #[test]
    fn candidate_score_is_intersection_with_archive_cached_tokens() {
        let prompt_tokens = tokenize("weather forecast Tallahassee");
        let mut store = ExternalContext::new();
        let scored = store.push(user("the weather in Tallahassee is sunny", 1));
        let unrelated = store.push(user("hello world friends", 1));
        // "weather" + "tallahassee" overlap; "the" is a
        // stopword.
        let stored = store.iter_stored().nth(scored).expect("present");
        assert_eq!(score_candidate(None, &prompt_tokens, stored), 2.0);
        let stored = store.iter_stored().nth(unrelated).expect("present");
        assert_eq!(score_candidate(None, &prompt_tokens, stored), 0.0);
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
        let policy =
            ContextVirtualizationPolicy::new(5, 1, 0, store, shared_pushed(HashSet::new()));
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
        let policy = ContextVirtualizationPolicy::new(
            5,
            1,
            50,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };

        // Find the Tallahassee message in survivors. With
        // FIFO + a 1-token pinned tail alone it would be
        // evicted; the reranker should have paged it back in.
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

    /// Phase E: paging-in respects the per-fire budget. Many
    /// matching candidates, a budget that fits exactly one
    /// rendered section — only one is recalled, and the spend
    /// counter stays at or under the budget.
    #[tokio::test]
    async fn paged_in_respects_budget() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let mut context: Vec<Message> = (0..10)
            .map(|i| user(&format!("weather report number {i}"), i as u64))
            .collect();
        context.push(user("what's the weather like?", 100));

        // Each rendered section ("[archive entry N]\n
        // weather report number N") is ~10 estimated tokens;
        // a 15-token budget admits one and rejects a second.
        let budget = 15_u64;
        let policy = ContextVirtualizationPolicy::new(
            2,
            1,
            budget,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
        let response = policy.before_model(sample_request(&context)).await;
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall = survivors
            .iter()
            .find(|m| is_archive_recall(m))
            .and_then(|m| user_text(m))
            .expect("expected at least one paged-in section");
        assert_eq!(
            recall.matches("[archive entry").count(),
            1,
            "budget admits exactly one section: {recall}"
        );
        let state = policy
            .page_in_state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            state.spent_tokens <= budget,
            "spent tokens ({}) exceeded budget ({budget})",
            state.spent_tokens
        );
        assert_eq!(state.sticky_sections.len(), 1);
    }

    /// Phase E: paging-in does not duplicate messages that
    /// are already in `working`. The reranker filters
    /// candidates by timestamp absence in working.
    #[tokio::test]
    async fn paged_in_excludes_active_context() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        // All 5 messages contain "weather" (high score
        // candidates). With a 10_000-token ceiling, no
        // eviction triggers, so ALL 5 are already in
        // working.
        let context: Vec<Message> = (0..5)
            .map(|i| user(&format!("weather weather {i}"), i as u64))
            .chain([user("what's the weather?", 100)])
            .collect();
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            6,
            1_000,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
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

        let policy = ContextVirtualizationPolicy::new(
            2,
            1,
            1_000,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
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

    /// rlm2/PR4: ALL paged content renders inside ONE
    /// consolidated `<system-reminder source="archive-recall">`
    /// message placed immediately before the ledger — never
    /// interleaved into the transcript as fake user turns.
    #[tokio::test]
    async fn paged_content_renders_in_one_archive_recall_message() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            user("weather note alpha", 1),
            user("weather note beta", 2),
        ])));
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            10_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64, 2u64])),
        );
        let context = vec![user("weather?", 100)];
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recalls: Vec<&str> = survivors
            .iter()
            .filter(|m| is_archive_recall(m))
            .filter_map(|m| user_text(m))
            .collect();
        assert_eq!(
            recalls.len(),
            1,
            "exactly one recall message: {survivors:?}"
        );
        assert!(recalls[0].contains("weather note alpha"), "{}", recalls[0]);
        assert!(recalls[0].contains("weather note beta"), "{}", recalls[0]);
        // Placement: immediately before the ledger, which
        // stays strictly last (invariant b).
        assert!(is_ledger(survivors.last().expect("non-empty")));
        assert!(is_archive_recall(&survivors[survivors.len() - 2]));
        // No fake interleaved turns: archive content appears
        // nowhere outside the recall message.
        for (idx, m) in survivors.iter().enumerate() {
            if idx != survivors.len() - 2 {
                assert!(
                    !user_text(m).unwrap_or("").contains("[archive entry"),
                    "archive content leaked outside the recall message at idx {idx}"
                );
            }
        }
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

        let summary = collect_tool_call_summary(&store, &HashSet::new());
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
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            0,
            Arc::clone(&store),
            shared_pushed(pushed),
        );
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

    /// rlm2/PR5: the ledger caps each tool's identity list
    /// at the MOST RECENT `TOOL_CALL_DISPLAY_CAP` entries;
    /// all elided older calls collapse into one trailing
    /// overflow line that names recurse message_grep. Keeps
    /// the ledger bounded even when the agent has fired
    /// hundreds of tool calls, without losing the pointer to
    /// the elided history.
    #[test]
    fn ledger_caps_per_tool_with_overflow_line() {
        let entries: Vec<ToolCallEntry> = (0..12)
            .map(|i| ToolCallEntry {
                tool_call_id: format!("tc_{i}"),
                arg_value: format!("https://example.com/page{i}"),
            })
            .collect();
        let summary = vec![("web_read".to_string(), entries.clone())];
        let lines = render_tool_call_summary_lines(&summary);
        assert_eq!(
            lines.len(),
            2,
            "one tool line + one overflow line: {lines:?}"
        );
        let line = &lines[0];
        // The most recent 8 entries appear with their ids;
        // the 4 oldest (page0..page3) do not.
        assert!(line.contains("page4 (id=tc_4)"), "{line}");
        assert!(line.contains("page11 (id=tc_11)"), "{line}");
        assert!(!line.contains("(id=tc_3)"), "{line}");
        assert!(!line.contains("(id=tc_0)"), "{line}");
        let overflow = &lines[1];
        assert!(overflow.contains("4 earlier calls"), "{overflow}");
        assert!(overflow.contains("message_grep"), "{overflow}");

        // The Small-tier renderer applies the same cap with
        // the no-syntax phrasing (no scope grammar).
        let plain = render_plain_tool_call_lines(&summary);
        assert_eq!(plain.len(), 2, "{plain:?}");
        assert!(
            plain[0].contains("\"https://example.com/page4\""),
            "{}",
            plain[0]
        );
        assert!(!plain[0].contains("page3\""), "{}", plain[0]);
        assert!(plain[1].contains("4 earlier calls"), "{}", plain[1]);
        assert!(plain[1].contains("recurse message_grep"), "{}", plain[1]);
        assert!(!plain[1].contains("scope.kind"), "{}", plain[1]);

        // No overflow → no overflow line (byte-stability:
        // the line appears exactly when calls are elided).
        let small = vec![("web_read".to_string(), entries[..3].to_vec())];
        assert_eq!(render_tool_call_summary_lines(&small).len(), 1);
        assert_eq!(render_plain_tool_call_lines(&small).len(), 1);
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

    /// rlm2/PR4: summaries-first — when a candidate has a
    /// Phase-F summary, the summary is what pages in, even
    /// when the full body would comfortably fit the budget.
    /// The section header tells the model recurse fetches
    /// the full body.
    #[tokio::test]
    async fn page_in_prefers_summary_over_body() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "relevant_keyword body content here",
            1,
        )])));
        store
            .write()
            .await
            .set_summary(0, "concise summary text".to_string());
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            10_000, // roomy budget — the body WOULD fit; preference decides
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        );
        let context = vec![user("looking for relevant_keyword info", 200)];
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall = survivors
            .iter()
            .find(|m| is_archive_recall(m))
            .and_then(|m| user_text(m))
            .expect("recall message present");
        assert!(recall.contains("concise summary text"), "{recall}");
        assert!(
            recall.contains("— summary; full body via recurse"),
            "summary sections must advertise the recurse path: {recall}"
        );
        assert!(
            !recall.contains("relevant_keyword body content here"),
            "the body must not page in when a summary exists: {recall}"
        );
    }

    /// rlm2/PR4: the `ANIE_PAGE_IN_BODIES=1` A/B hatch
    /// restores the pre-PR4 preference — full body first when
    /// it fits the budget, summary as the fallback.
    #[tokio::test]
    async fn page_in_bodies_env_hatch_restores_body_preference() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "relevant_keyword body content here",
            1,
        )])));
        store
            .write()
            .await
            .set_summary(0, "concise summary text".to_string());
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            10_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        )
        .with_page_in_bodies(true);
        let context = vec![user("looking for relevant_keyword info", 200)];
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall = survivors
            .iter()
            .find(|m| is_archive_recall(m))
            .and_then(|m| user_text(m))
            .expect("recall message present");
        assert!(
            recall.contains("relevant_keyword body content here"),
            "bodies hatch pages the full body: {recall}"
        );
        assert!(
            !recall.contains("concise summary text"),
            "bodies hatch must not also page the summary: {recall}"
        );
    }

    /// rlm2/PR4: a large unsummarized body (≥ 512 estimated
    /// tokens) never pages in, even when both the per-fire
    /// and per-run budgets would admit it — it stays
    /// reachable via recurse only.
    #[tokio::test]
    async fn large_unsummarized_body_is_not_paged_in() {
        let huge_text: String = format!("relevant_keyword {}", "filler ".repeat(600));
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            &huge_text, 1,
        )])));
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            100_000, // the old behavior would have paged the body in
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        );
        let context = vec![user("looking for relevant_keyword info", 200)];
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        assert!(
            !survivors.iter().any(is_archive_recall),
            "large unsummarized body must stay recurse-only: {survivors:?}"
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
        }
        let embedder: Arc<dyn crate::embedder::Embedder> = Arc::new(StubEmbedder);
        let (tx, _rx) = mpsc::channel::<EmbedRequest>(8);

        let pushed = HashSet::from([1u64]);
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            2,
            10_000,
            Arc::clone(&store),
            shared_pushed(pushed),
        )
        .with_embedder(embedder, tx);

        // Active context's prompt has no keyword overlap
        // with the candidate. With keyword scoring this
        // would page in nothing; with embedding cosine=1
        // it should page in. RC2: the prompt embedding is
        // computed by a background task — the first fire
        // falls back to keyword overlap (finding nothing)
        // and kicks the embed off; a later fire picks the
        // cached vector up and pages the candidate in by
        // cosine.
        let context = vec![user("totally different abc query", 100)];
        let mut paged_in = false;
        for _ in 0..100 {
            let response = policy.before_model(sample_request(&context)).await;
            let survivors = match response {
                BeforeModelResponse::ReplaceMessages(s) => s,
                other => panic!("expected ReplaceMessages, got {other:?}"),
            };
            if survivors.iter().any(|m| match m {
                Message::User(u) => match u.content.first() {
                    Some(ContentBlock::Text { text }) => text.contains("zero keyword overlap"),
                    _ => false,
                },
                _ => false,
            }) {
                paged_in = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            paged_in,
            "embedding cosine=1 candidate should be paged in once the background prompt embed lands"
        );
    }

    /// RC2 regression (docs/code_review_2026-06-11.md): a
    /// cache-miss prompt embed must never be awaited on the
    /// before_model path. With an embedder that takes 30s,
    /// the fire must return promptly — reranking by keyword
    /// overlap for the current fire — while the embed runs
    /// in the background.
    #[tokio::test]
    async fn prompt_embed_cache_miss_does_not_block_before_model() {
        use crate::bg_embedder::EmbedRequest;

        struct SlowEmbedder;
        #[async_trait::async_trait]
        impl crate::embedder::Embedder for SlowEmbedder {
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                Ok(vec![1.0, 0.0, 0.0])
            }
        }

        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "relevant_keyword content here",
            1,
        )])));
        let (tx, _rx) = mpsc::channel::<EmbedRequest>(8);
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            2,
            10_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        )
        .with_embedder(Arc::new(SlowEmbedder), tx);

        let context = vec![user("looking for relevant_keyword", 100)];
        let started = std::time::Instant::now();
        let response = policy.before_model(sample_request(&context)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "before_model must not await the prompt embed inline"
        );
        // Keyword fallback still reranks on this fire.
        let survivors = match response {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let has_candidate = survivors.iter().any(|m| match m {
            Message::User(u) => match u.content.first() {
                Some(ContentBlock::Text { text }) => text.contains("relevant_keyword content here"),
                _ => false,
            },
            _ => false,
        });
        assert!(
            has_candidate,
            "keyword overlap must still page in while the embed is pending: {survivors:?}"
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
        }
        let embedder: Arc<dyn crate::embedder::Embedder> = Arc::new(OrthogonalEmbedder);
        let (tx, _rx) = mpsc::channel::<EmbedRequest>(8);

        let pushed = HashSet::from([1u64, 2u64]);
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            2,
            10_000,
            Arc::clone(&store),
            shared_pushed(pushed),
        )
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
            shared_pushed(HashSet::from([1u64])),
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
            content: vec![ContentBlock::Text {
                text: "[tool error] failed".into(),
            }],
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
        assert_eq!(
            supersedable,
            vec![1],
            "only the failed result at idx 1 supersedable"
        );
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
        assert!(
            supersedable.is_empty(),
            "different args should not supersede"
        );
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

    // ---- PR 4 Signal B: stale failure detection ----

    #[test]
    fn stale_failure_detected_when_far_from_tail() {
        // Failure at idx 0, plenty of messages after it, NOT
        // in the pinned tail.
        let working = vec![
            failed_tool_result("c1", "bash", 1), // idx 0 — stale
            user("dummy", 2),
            user("dummy", 3),
            user("dummy", 4),
            user("dummy", 5),
            user("latest", 6), // pinned tail
        ];
        // Pinned tail = the last message only.
        let stale = find_stale_failures(&working, 5);
        assert_eq!(stale, vec![0]);
    }

    #[test]
    fn fresh_failure_not_marked_stale() {
        // Failure with too few messages after it (still fresh).
        let working = vec![
            user("first", 1),
            user("second", 2),
            failed_tool_result("c1", "bash", 3), // only 2 after
            user("third", 4),
            user("latest", 5),
        ];
        let stale = find_stale_failures(&working, 4);
        // Failure at idx 2: messages_after = 5 - 2 = 3, < 4 threshold.
        assert!(stale.is_empty());
    }

    #[test]
    fn stale_failure_in_pinned_tail_skipped() {
        // Failure inside the pinned-tail window — even if old
        // enough, the tail pin protects it.
        let working = vec![
            user("a", 1),
            user("b", 2),
            user("c", 3),
            user("d", 4),
            user("e", 5),
            failed_tool_result("c1", "bash", 6), // last; pinned
        ];
        // Pinned tail is the last 2 (start index 4). Idx 5
        // is in the tail.
        let stale = find_stale_failures(&working, 4);
        assert!(stale.is_empty());
    }

    #[test]
    fn multiple_stale_failures_returned_in_order() {
        let working = vec![
            failed_tool_result("c1", "bash", 1), // idx 0 — stale
            user("a", 2),
            failed_tool_result("c2", "edit", 3), // idx 2 — stale
            user("b", 4),
            user("c", 5),
            user("d", 6),
            user("e", 7),
            user("latest", 8), // pinned
        ];
        let stale = find_stale_failures(&working, 7);
        assert_eq!(stale, vec![0, 2]);
    }

    #[test]
    fn successful_results_never_marked_stale() {
        let working = vec![
            ok_tool_result("c1", "bash", 1), // idx 0 — NOT stale
            user("a", 2),
            user("b", 3),
            user("c", 4),
            user("d", 5),
            user("latest", 6),
        ];
        let stale = find_stale_failures(&working, 5);
        assert!(stale.is_empty(), "successes should not be stale-eligible");
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

    // ----- rlm2/PR2: silent-truncation alarm -----

    /// An assistant reply as the Ollama provider would report
    /// it: `usage.input_tokens` carries `prompt_eval_count`.
    fn reply_from(provider: &str, prefill_tokens: u64, ts: u64) -> Message {
        reply_stopped(provider, prefill_tokens, ts, StopReason::Stop)
    }

    fn reply_stopped(
        provider: &str,
        prefill_tokens: u64,
        ts: u64,
        stop_reason: StopReason,
    ) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![ContentBlock::Text { text: "ok".into() }],
            usage: Usage {
                input_tokens: prefill_tokens,
                ..Usage::default()
            },
            stop_reason,
            error_message: None,
            provider: provider.into(),
            model: "test".into(),
            timestamp: ts,
            reasoning_details: None,
        })
    }

    /// The effective `num_ctx` the alarm-test policies declare.
    /// The "long prompt" contexts below estimate at ~300 tokens,
    /// so a send always exceeds this window; a prefill of
    /// [`SHIFT_PREFILL`] (≥ half the window, far under the sent
    /// estimate) is the genuine context-shift signature.
    const ALARM_NUM_CTX: u64 = 100;
    const SHIFT_PREFILL: u64 = 90;

    /// Policy with a comfortable ceiling (no eviction → no
    /// breadcrumb SystemMessages to confuse the assertions),
    /// a declared Ollama window for the shift detector, plus an
    /// event channel for the alarm.
    fn alarm_policy() -> (ContextVirtualizationPolicy, mpsc::Receiver<AgentEvent>) {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let (tx, rx) = mpsc::channel(32);
        let policy =
            ContextVirtualizationPolicy::new(10_000, 4, 0, store, shared_pushed(HashSet::new()))
                .with_ollama_num_ctx(Some(ALARM_NUM_CTX))
                .with_event_sender(tx);
        (policy, rx)
    }

    fn drain_system_messages(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<String> {
        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::SystemMessage { text } = event {
                texts.push(text);
            }
        }
        texts
    }

    /// The alarm consumes the PR1 detector: an Ollama reply
    /// carrying the context-shift signature (send above the
    /// window, prefill near the window — a forced re-eval of the
    /// shifted prefix) emits the remediation `SystemMessage` —
    /// exactly once per run, even when later turns keep
    /// truncating.
    #[tokio::test]
    async fn truncation_alarm_fires_once_per_run() {
        let (policy, mut rx) = alarm_policy();

        // Fire 1: establishes the sent-context baseline.
        let mut context = vec![user(&"long prompt ".repeat(100), 1)];
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "no alarm before any reply exists"
        );

        // Ollama replies having evaluated ~the whole 100-token
        // window — far under the ~300-token send: context shift.
        context.push(reply_from(
            crate::run_metrics::OLLAMA_PROVIDER,
            SHIFT_PREFILL,
            2,
        ));
        policy.before_model(sample_request(&context)).await;
        let alarms = drain_system_messages(&mut rx);
        assert_eq!(
            alarms.len(),
            1,
            "alarm fires on the truncated turn: {alarms:?}"
        );
        assert!(alarms[0].contains("silently truncated"), "{}", alarms[0]);
        assert!(alarms[0].contains("/context-length"), "{}", alarms[0]);
        assert!(
            alarms[0].contains("ANIE_ACTIVE_CEILING_TOKENS"),
            "{}",
            alarms[0]
        );

        // A second truncated turn still WARNs (not asserted)
        // but must NOT re-emit the SystemMessage.
        context.push(reply_from(
            crate::run_metrics::OLLAMA_PROVIDER,
            SHIFT_PREFILL,
            3,
        ));
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "the SystemMessage is one-time-per-run"
        );
    }

    /// Regression (rlm2 review): Ollama's `prompt_eval_count`
    /// counts only newly-evaluated tokens, so an append-only turn
    /// served from the prefix cache legitimately reports a tiny
    /// prefill (the new suffix) or omits the field entirely
    /// (`input_tokens` = 0). Neither is a truncation — the old
    /// bare-undershoot predicate fired on every healthy cached
    /// turn, exactly when the PR3 fast path was working.
    #[tokio::test]
    async fn cached_prefix_prefill_does_not_trip_truncation_alarm() {
        let (policy, mut rx) = alarm_policy();
        let mut context = vec![user(&"long prompt ".repeat(100), 1)];
        policy.before_model(sample_request(&context)).await;
        drain_system_messages(&mut rx);

        // Suffix-only prefill, far below the declared window.
        context.push(reply_from(crate::run_metrics::OLLAMA_PROVIDER, 10, 2));
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "a suffix-only prefill is a cache hit, not a truncation"
        );

        // Fully-cached prompt: Ollama omits prompt_eval_count.
        context.push(reply_from(crate::run_metrics::OLLAMA_PROVIDER, 0, 3));
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "an omitted prompt_eval_count is a cache hit, not a truncation"
        );
    }

    /// Without a declared `num_ctx` (non-native APIs) a prefill
    /// undershoot can't be told apart from a cache hit — the
    /// alarm stays off.
    #[tokio::test]
    async fn truncation_alarm_off_without_a_known_num_ctx() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let (tx, mut rx) = mpsc::channel(32);
        let policy =
            ContextVirtualizationPolicy::new(10_000, 4, 0, store, shared_pushed(HashSet::new()))
                .with_event_sender(tx);
        let mut context = vec![user(&"long prompt ".repeat(100), 1)];
        policy.before_model(sample_request(&context)).await;
        drain_system_messages(&mut rx);

        context.push(reply_from(
            crate::run_metrics::OLLAMA_PROVIDER,
            SHIFT_PREFILL,
            2,
        ));
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "no num_ctx → no shift detection"
        );
    }

    /// Regression (rlm2 review): the agent loop synthesizes
    /// assistant messages for stream failures and aborts with the
    /// real provider string, a fresh timestamp, and defaulted (or
    /// partial) usage. Those never evaluated the send — they must
    /// not be read as prefill samples, even when their usage would
    /// otherwise match the shift signature.
    #[tokio::test]
    async fn errored_or_aborted_replies_do_not_trip_truncation_alarm() {
        for stop_reason in [StopReason::Error, StopReason::Aborted] {
            let (policy, mut rx) = alarm_policy();
            let mut context = vec![user(&"long prompt ".repeat(100), 1)];
            policy.before_model(sample_request(&context)).await;
            drain_system_messages(&mut rx);

            context.push(reply_stopped(
                crate::run_metrics::OLLAMA_PROVIDER,
                SHIFT_PREFILL,
                2,
                stop_reason,
            ));
            policy.before_model(sample_request(&context)).await;
            assert!(
                drain_system_messages(&mut rx).is_empty(),
                "{stop_reason:?} replies are not prefill samples"
            );
        }
    }

    /// A prefill count near the estimate is inside the
    /// heuristic band — no alarm.
    #[tokio::test]
    async fn truncation_alarm_quiet_when_prefill_matches_estimate() {
        let (policy, mut rx) = alarm_policy();
        let mut context = vec![user(&"long prompt ".repeat(100), 1)];
        policy.before_model(sample_request(&context)).await;
        drain_system_messages(&mut rx);

        // Recover the exact estimate the policy reported, and
        // reply with a prefill right at it.
        let sent = 1_000; // comfortably above the real ~300-token estimate
        context.push(reply_from(crate::run_metrics::OLLAMA_PROVIDER, sent, 2));
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "prefill ≥ estimate is not a truncation"
        );
    }

    /// Hosted providers have no `prompt_eval_count` semantics —
    /// a low `input_tokens` there is not a truncation, so the
    /// alarm never fires for non-Ollama replies.
    #[tokio::test]
    async fn truncation_alarm_never_fires_for_non_ollama_provider() {
        let (policy, mut rx) = alarm_policy();
        let mut context = vec![user(&"long prompt ".repeat(100), 1)];
        policy.before_model(sample_request(&context)).await;
        drain_system_messages(&mut rx);

        // Shift-shaped numbers, but a hosted provider string.
        context.push(reply_from("openai", SHIFT_PREFILL, 2));
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "hosted providers are exempt from the alarm"
        );
    }

    /// A retried turn (model call errored, so no fresh
    /// assistant reply landed) must not compare the new
    /// estimate against a stale reply's prefill count.
    #[tokio::test]
    async fn truncation_alarm_requires_a_fresh_assistant_reply() {
        let (policy, mut rx) = alarm_policy();
        // The context already ends with an old Ollama reply whose
        // prefill would match the shift signature against the NEW
        // send's estimate — only the freshness guard protects it.
        let mut context = vec![
            user("hi", 1),
            reply_from(crate::run_metrics::OLLAMA_PROVIDER, SHIFT_PREFILL, 2),
        ];
        context.push(user(&"long prompt ".repeat(100), 3));
        policy.before_model(sample_request(&context)).await;
        drain_system_messages(&mut rx);

        // Fire again with NO new assistant message (the retry
        // case): the stale reply's prefill must not be read as
        // a truncation of the just-measured send.
        policy.before_model(sample_request(&context)).await;
        assert!(
            drain_system_messages(&mut rx).is_empty(),
            "stale replies are not prefill samples for the new send"
        );
    }

    // ----- rlm2/PR3: batch eviction + append-only turns -----

    /// What the agent loop does with a policy response: keep
    /// the context on `Continue`, adopt the replacement on
    /// `ReplaceMessages`. The PR3 tests drive multi-turn
    /// sequences through this so the byte-compare assertions
    /// exercise the same state transitions as the real loop.
    fn apply_response(response: BeforeModelResponse, context: Vec<Message>) -> Vec<Message> {
        match response {
            BeforeModelResponse::Continue => context,
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected Continue or ReplaceMessages, got {other:?}"),
        }
    }

    #[test]
    fn low_water_pct_unset_uses_default() {
        assert_eq!(resolve_evict_low_water_pct(None), 0.6);
    }

    #[test]
    fn low_water_pct_clamped_into_bounds() {
        assert_eq!(resolve_evict_low_water_pct(Some("0.1")), 0.3);
        assert_eq!(resolve_evict_low_water_pct(Some("0.99")), 0.9);
        assert_eq!(resolve_evict_low_water_pct(Some("0.75")), 0.75);
    }

    #[test]
    fn low_water_pct_unparseable_falls_back_to_default() {
        assert_eq!(resolve_evict_low_water_pct(Some("lots")), 0.6);
        assert_eq!(resolve_evict_low_water_pct(Some("")), 0.6);
        // `"NaN".parse::<f64>()` succeeds; the finite guard
        // (not the parse) has to catch it — clamp on NaN
        // would propagate NaN into the eviction target.
        assert_eq!(resolve_evict_low_water_pct(Some("NaN")), 0.6);
        assert_eq!(resolve_evict_low_water_pct(Some("inf")), 0.6);
    }

    /// rlm2/PR3: a retried turn — same context as the previous
    /// fire, nothing new to archive, rebuilt ledger identical —
    /// returns `Continue` and leaves the in-place ledger alone
    /// instead of stripping and re-appending identical bytes.
    #[tokio::test]
    async fn under_ceiling_turn_with_no_new_archive_returns_continue() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            4,
            0,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
        let context = vec![user("hi", 1), assistant("hello", 2)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        assert!(is_ledger(sent_1.last().expect("non-empty")));

        let response = policy.before_model(sample_request(&sent_1)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
        // Nothing new arrived, so the archive holds exactly the
        // two originals (the ledger is never archived).
        assert_eq!(store.read().await.len(), 2);
    }

    /// rlm2/PR3 hysteresis: a ceiling breach evicts in one
    /// batch down to `ceiling × low_water_pct`, not merely to
    /// the ceiling — otherwise the very next append breaches
    /// again and every turn pays an eviction + ledger rebuild.
    #[tokio::test]
    async fn eviction_batches_down_to_low_water_mark() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let body = "x".repeat(40); // 10 estimated tokens per message
        let context: Vec<Message> = (0..30).map(|i| user(&body, i as u64)).collect();
        let ceiling = 200u64;
        let policy =
            ContextVirtualizationPolicy::new(ceiling, 2, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let surviving_tokens: u64 = survivors
            .iter()
            .filter(|m| !is_ledger(m))
            .map(estimate_tokens)
            .fold(0, u64::saturating_add);
        let low_water = (ceiling as f64 * 0.6) as u64;
        assert!(
            surviving_tokens <= low_water,
            "eviction must batch down to the low-water mark ({low_water}), \
             not stop at the ceiling ({ceiling}); got {surviving_tokens}"
        );
        // ... but no further than the low-water mark: the
        // first message that lands the total at or under the
        // target is the last one evicted.
        assert!(
            surviving_tokens + 10 > low_water,
            "eviction overshot the low-water mark ({low_water}): {surviving_tokens}"
        );
    }

    /// rlm2/PR3, the byte-compare test: two consecutive
    /// appending turns (plain assistant replies, no new tool
    /// activity) take the `Continue` fast path, so each turn's
    /// outgoing context is a pure extension of the previous
    /// turn's — the pre-ledger prefix AND the ledger bytes are
    /// unchanged (Ollama's prefix cache holds). The stability
    /// property must hold for both ledger formats, so the
    /// scenario runs against the v1 (Full-tier) and v2
    /// (Small-tier) renderers.
    #[tokio::test]
    async fn ledger_bytes_stable_across_appending_turns() {
        for small_tier in [false, true] {
            let store = Arc::new(RwLock::new(ExternalContext::new()));
            let policy = ContextVirtualizationPolicy::new(
                10_000,
                4,
                0,
                Arc::clone(&store),
                shared_pushed(HashSet::new()),
            )
            .with_small_tier_ledger(small_tier);

            // Turn 1: first fire injects the ledger.
            let context = vec![user("original question", 1), assistant("working on it", 2)];
            let sent_1 =
                apply_response(policy.before_model(sample_request(&context)).await, context);
            assert!(is_ledger(sent_1.last().expect("non-empty")));
            let ledger_text_1 = user_text(sent_1.last().expect("non-empty"))
                .expect("ledger text")
                .to_string();

            // Turn 2: the model replied with plain text; the
            // loop appends it after the ledger.
            let mut context_2 = sent_1.clone();
            context_2.push(assistant("a plain text reply", 3));
            let response_2 = policy.before_model(sample_request(&context_2)).await;
            assert_eq!(
                response_2,
                BeforeModelResponse::Continue,
                "appending turn must not rebuild (small_tier={small_tier})"
            );
            let sent_2 = apply_response(response_2, context_2);
            // Invariant (c): the new message was archived
            // store-side even though the fast path returned
            // Continue.
            assert_eq!(store.read().await.len(), 3, "small_tier={small_tier}");

            // Turn 3: another plain append.
            let mut context_3 = sent_2.clone();
            context_3.push(assistant("another plain reply", 4));
            let response_3 = policy.before_model(sample_request(&context_3)).await;
            assert_eq!(
                response_3,
                BeforeModelResponse::Continue,
                "small_tier={small_tier}"
            );
            let sent_3 = apply_response(response_3, context_3);
            assert_eq!(store.read().await.len(), 4, "small_tier={small_tier}");

            // The byte compare: every send extends the previous
            // send without touching it.
            assert_eq!(&sent_2[..sent_1.len()], &sent_1[..]);
            assert_eq!(&sent_3[..sent_2.len()], &sent_2[..]);
            // Exactly one ledger survives, byte-identical to
            // the one turn 1 injected.
            let ledgers: Vec<&Message> = sent_3.iter().filter(|m| is_ledger(m)).collect();
            assert_eq!(ledgers.len(), 1, "small_tier={small_tier}");
            assert_eq!(
                user_text(ledgers[0]).expect("ledger text"),
                ledger_text_1,
                "small_tier={small_tier}"
            );
        }
    }

    /// rlm2/PR3 negative control: an appending turn that DOES
    /// change the ledger content (a new tool call + result
    /// landed in the archive) rebuilds — the stale ledger is
    /// stripped and the fresh one is strictly the last
    /// message, while the pre-ledger prefix is preserved.
    #[tokio::test]
    async fn appending_turn_with_new_tool_activity_rebuilds_ledger() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            0,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
        let context = vec![user("original question", 1), assistant("working on it", 2)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        let ledger_text_1 = user_text(sent_1.last().expect("non-empty"))
            .expect("ledger text")
            .to_string();

        let mut context_2 = sent_1.clone();
        context_2.push(assistant_with_tool_call(
            "bash",
            serde_json::json!({"command": "ls"}),
            3,
        ));
        context_2.push(tool_result("call_3", "bash", "file listing", 4));
        let survivors = match policy.before_model(sample_request(&context_2)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("tool activity must force a rebuild, got {other:?}"),
        };
        // [user, assistant, tool-call assistant, tool result, ledger].
        assert_eq!(survivors.len(), 5);
        assert!(is_ledger(survivors.last().expect("non-empty")));
        assert_eq!(survivors.iter().filter(|m| is_ledger(m)).count(), 1);
        // Pre-ledger prefix preserved.
        assert_eq!(&survivors[..2], &sent_1[..2]);
        let new_ledger_text = user_text(survivors.last().expect("non-empty")).expect("text");
        assert_ne!(new_ledger_text, ledger_text_1);
        assert!(new_ledger_text.contains("bash"), "{new_ledger_text}");
    }

    /// rlm2/PR3: when a turn evicts and pages content back in,
    /// the rebuilt ledger is still strictly the last message
    /// (paged-in messages re-sort into chronology *before* the
    /// ledger is appended).
    #[tokio::test]
    async fn ledger_remains_last_message_after_page_in() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let mut context: Vec<Message> = vec![
            user("a long discussion about Tallahassee weather patterns", 1),
            user("filler about pets", 2),
            user("filler about food", 3),
            user("filler about music", 4),
            user("filler about books", 5),
        ];
        context.push(user("what's the weather in Tallahassee tomorrow?", 100));
        let policy = ContextVirtualizationPolicy::new(
            5,
            1,
            50,
            Arc::clone(&store),
            shared_pushed(HashSet::new()),
        );
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let paged_in = survivors.iter().any(|m| {
            user_text(m)
                .map(|t| t.contains("Tallahassee weather patterns"))
                .unwrap_or(false)
        });
        assert!(paged_in, "the test needs a page-in to be meaningful");
        assert!(is_ledger(survivors.last().expect("non-empty")));
        assert_eq!(survivors.iter().filter(|m| is_ledger(m)).count(), 1);
    }

    /// rlm2/PR2 interaction: the silent-truncation alarm and
    /// the baseline recording both still run on the PR3
    /// append-only fast path — the alarm at the start of the
    /// fire, the baseline right before `Continue` returns. The
    /// reply carries the genuine shift signature (prefill ≈ the
    /// declared window, far under the send) — a suffix-sized
    /// prefill on this same path is the healthy cached case and
    /// must NOT fire (`cached_prefix_prefill_does_not_trip_truncation_alarm`).
    #[tokio::test]
    async fn truncation_alarm_still_fires_on_append_only_noop_turn() {
        let (policy, mut rx) = alarm_policy();
        let context = vec![user(&"long prompt ".repeat(100), 1)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        drain_system_messages(&mut rx);

        // The Ollama reply re-evaluated ~the whole window — far
        // under what the previous fire sent — and, being a plain
        // text append, the turn takes the Continue fast path.
        let mut context_2 = sent_1;
        context_2.push(reply_from(
            crate::run_metrics::OLLAMA_PROVIDER,
            SHIFT_PREFILL,
            2,
        ));
        let response = policy.before_model(sample_request(&context_2)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
        let alarms = drain_system_messages(&mut rx);
        assert_eq!(
            alarms.len(),
            1,
            "alarm must fire on the fast path: {alarms:?}"
        );
        assert!(alarms[0].contains("silently truncated"), "{}", alarms[0]);

        // And the fast path recorded a fresh baseline for the
        // NEXT turn's comparison (sized to what actually went
        // out: the unchanged request context).
        let baseline = policy
            .pending_truncation_check
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .expect("baseline recorded on the Continue path");
        let expected: u64 = context_2
            .iter()
            .map(estimate_tokens)
            .fold(0, u64::saturating_add);
        assert_eq!(baseline.sent_context_tokens, expected);
    }

    // ----- rlm2/PR4: summaries-first, sticky page-ins -----

    /// The consolidated archive-recall message in `survivors`,
    /// if any.
    fn recall_text_of(survivors: &[Message]) -> Option<String> {
        survivors
            .iter()
            .find(|m| is_archive_recall(m))
            .and_then(|m| user_text(m))
            .map(str::to_string)
    }

    #[test]
    fn page_in_run_budget_unset_uses_default() {
        assert_eq!(resolve_page_in_run_budget(None), 8192);
    }

    #[test]
    fn page_in_run_budget_parses_value_and_rejects_garbage() {
        assert_eq!(resolve_page_in_run_budget(Some("4096")), 4096);
        assert_eq!(resolve_page_in_run_budget(Some(" 0 ")), 0);
        assert_eq!(resolve_page_in_run_budget(Some("lots")), 8192);
        assert_eq!(resolve_page_in_run_budget(Some("")), 8192);
        assert_eq!(resolve_page_in_run_budget(Some("-1")), 8192);
    }

    #[test]
    fn page_in_bodies_flag_recognizes_truthy_values_only() {
        assert!(!resolve_page_in_bodies(None));
        assert!(resolve_page_in_bodies(Some("1")));
        assert!(resolve_page_in_bodies(Some("true")));
        assert!(resolve_page_in_bodies(Some("yes")));
        assert!(!resolve_page_in_bodies(Some("0")));
        assert!(!resolve_page_in_bodies(Some("false")));
        assert!(!resolve_page_in_bodies(Some("")));
    }

    /// rlm2/PR4: a recalled item is sticky — it stays in the
    /// recall message across later turns (including turns
    /// that FIFO-evict working content) and disappears only
    /// when the latest user prompt changes.
    #[tokio::test]
    async fn sticky_page_in_survives_fifo_until_prompt_changes() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "archived Tallahassee weather discussion",
            1,
        )])));
        let policy = ContextVirtualizationPolicy::new(
            60,
            1,
            1_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        )
        .with_evict_low_water_pct(0.6);

        // Fire 1: the prompt matches the archived candidate.
        let context = vec![user("what was the Tallahassee weather conclusion?", 100)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        let recall_1 = recall_text_of(&sent_1).expect("recall after fire 1");
        assert!(recall_1.contains("archived Tallahassee weather discussion"));

        // Fire 2: bulky filler breaches the ceiling, so FIFO
        // eviction runs. The recalled item never entered the
        // working set, so eviction can't touch it — the
        // rebuilt recall still carries it, byte-identical.
        let mut context_2 = sent_1.clone();
        for i in 0..6u64 {
            context_2.push(assistant(&"filler chatter ".repeat(10), 101 + i));
        }
        let survivors = match policy.before_model(sample_request(&context_2)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall_2 = recall_text_of(&survivors).expect("recall survives eviction");
        assert_eq!(recall_2, recall_1, "sticky recall re-renders identically");
        assert!(is_ledger(survivors.last().expect("non-empty")));
        assert!(is_archive_recall(&survivors[survivors.len() - 2]));

        // Fire 3: a NEW user prompt (different timestamp,
        // unrelated topic) resets the sticky set — the recall
        // message disappears.
        let mut context_3 = survivors.clone();
        context_3.push(user("completely unrelated cooking question", 200));
        let survivors_3 = match policy.before_model(sample_request(&context_3)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("prompt change must rebuild, got {other:?}"),
        };
        assert!(
            recall_text_of(&survivors_3).is_none(),
            "sticky set resets when the prompt changes: {survivors_3:?}"
        );
        assert!(is_ledger(survivors_3.last().expect("non-empty")));
    }

    /// rlm2/PR4: the same archive item is never paged in
    /// twice for one prompt — a later rebuilding turn neither
    /// duplicates the section nor re-charges the run budget.
    #[tokio::test]
    async fn same_item_not_paged_twice_for_one_prompt() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "relevant_keyword archived note",
            1,
        )])));
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            10_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        );
        let context = vec![user("looking for relevant_keyword", 100)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        assert!(recall_text_of(&sent_1).is_some());
        let spent_after_1 = {
            let state = policy
                .page_in_state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            assert_eq!(state.sticky_sections.len(), 1);
            assert!(state.spent_tokens > 0);
            state.spent_tokens
        };

        // New tool activity forces a ledger rebuild (the
        // recall path runs again), same prompt.
        let mut context_2 = sent_1.clone();
        context_2.push(assistant_with_tool_call(
            "bash",
            serde_json::json!({"command": "ls"}),
            101,
        ));
        context_2.push(tool_result("call_101", "bash", "listing", 102));
        let survivors = match policy.before_model(sample_request(&context_2)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall = recall_text_of(&survivors).expect("recall persists");
        assert_eq!(
            recall.matches("[archive entry").count(),
            1,
            "section must not duplicate: {recall}"
        );
        let state = policy
            .page_in_state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert_eq!(state.sticky_sections.len(), 1);
        assert_eq!(
            state.spent_tokens, spent_after_1,
            "re-rendering must not re-charge the run budget"
        );
    }

    /// rlm2/PR4: the per-run page-in budget caps total spend
    /// across the whole run, and resets when a new prompt
    /// (new run) arrives.
    #[tokio::test]
    async fn per_run_page_in_budget_is_enforced() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            user("weather note alpha", 1),
            user("weather note beta", 2),
        ])));
        // Each rendered section is ~9 estimated tokens; a
        // 12-token run budget admits one, then exhausts —
        // even though the per-fire relevance budget (10k)
        // would happily take both.
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            10_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64, 2u64])),
        )
        .with_page_in_run_budget(12);
        let context = vec![user("weather?", 100)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        {
            let state = policy
                .page_in_state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                state.sticky_sections.len(),
                1,
                "run budget admits one section"
            );
            assert!(state.spent_tokens <= 12);
        }

        // A rebuilding turn within the same run can't spend
        // past the cap: the second candidate stays out.
        let mut context_2 = sent_1.clone();
        context_2.push(assistant_with_tool_call(
            "bash",
            serde_json::json!({"command": "ls"}),
            101,
        ));
        context_2.push(tool_result("call_101", "bash", "listing", 102));
        let survivors = match policy.before_model(sample_request(&context_2)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        {
            let state = policy
                .page_in_state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            assert_eq!(
                state.sticky_sections.len(),
                1,
                "budget exhausted — no further page-ins this run"
            );
        }

        // A NEW prompt is a new run: the budget resets and
        // the remaining candidate can page in.
        let mut context_3 = survivors;
        context_3.push(user("more weather please", 200));
        let survivors_3 = match policy.before_model(sample_request(&context_3)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall_3 = recall_text_of(&survivors_3).expect("new run pages in again");
        assert_eq!(recall_3.matches("[archive entry").count(), 1);
        let state = policy
            .page_in_state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(state.spent_tokens <= 12, "fresh run, fresh budget");
    }

    /// rlm2/PR4 × PR3: a turn whose only change would be
    /// re-rendering an IDENTICAL archive-recall message still
    /// qualifies for the append-only no-op fast path — and
    /// the new message is archived store-side regardless
    /// (invariant c).
    #[tokio::test]
    async fn turn_after_page_in_with_identical_recall_takes_noop_fast_path() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![user(
            "relevant_keyword archived note",
            1,
        )])));
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            4,
            10_000,
            Arc::clone(&store),
            shared_pushed(HashSet::from([1u64])),
        );
        let context = vec![user("looking for relevant_keyword", 100)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        assert!(recall_text_of(&sent_1).is_some());
        assert!(is_ledger(sent_1.last().expect("non-empty")));

        // Plain text append: nothing evicts, nothing new
        // pages in, the rebuilt ledger and the re-rendered
        // recall are byte-identical — Continue, recall and
        // ledger left in place.
        let mut context_2 = sent_1.clone();
        context_2.push(assistant("a plain text reply", 101));
        let response = policy.before_model(sample_request(&context_2)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
        // Invariant (c): the reply was archived even though
        // the turn no-opped (candidate + prompt + reply).
        assert_eq!(store.read().await.len(), 3);

        // And the recall/ledger pair is stripped cleanly by a
        // later rebuilding turn: exactly one of each remains,
        // ledger strictly last, recall immediately before it.
        let mut context_3 = context_2.clone();
        context_3.push(assistant_with_tool_call(
            "bash",
            serde_json::json!({"command": "ls"}),
            102,
        ));
        context_3.push(tool_result("call_102", "bash", "listing", 103));
        let survivors = match policy.before_model(sample_request(&context_3)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        assert_eq!(survivors.iter().filter(|m| is_archive_recall(m)).count(), 1);
        assert_eq!(survivors.iter().filter(|m| is_ledger(m)).count(), 1);
        assert!(is_ledger(survivors.last().expect("non-empty")));
        assert!(is_archive_recall(&survivors[survivors.len() - 2]));
    }

    // ----- rlm2/PR5: ledger diet, token-budget tail, size-aware eviction -----

    /// rlm2/PR5: the pinned tail is a token budget, not a
    /// message count. 10 messages × 20 tokens with a 45-token
    /// tail budget pins exactly the last two messages (a
    /// third would overflow); under the old positional
    /// reading, "45" would have pinned the whole context and
    /// disabled eviction entirely.
    #[tokio::test]
    async fn pinned_tail_is_token_budgeted_not_positional() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let body = "y".repeat(80); // 20 estimated tokens per message
        let context: Vec<Message> = (0..10).map(|i| user(&body, i as u64)).collect();
        // Total ≈ 200; ceiling 100 → low-water target 60.
        let policy =
            ContextVirtualizationPolicy::new(100, 45, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let originals: Vec<&Message> = survivors.iter().filter(|m| !is_ledger(m)).collect();
        assert!(
            originals.len() < context.len(),
            "a 45-token budget must not pin all ten 20-token messages: {survivors:?}"
        );
        // FIFO stops at the low-water mark: 7 evicted, the
        // newest 3 (60 tokens) survive in order.
        assert_eq!(originals.len(), 3);
        let ts: Vec<u64> = originals.iter().map(|m| message_timestamp(m)).collect();
        assert_eq!(ts, vec![7, 8, 9]);
    }

    /// rlm2/PR5: the latest user AND latest assistant
    /// messages are identity-pinned regardless of the tail
    /// budget — with `pin_tail_tokens = 0` and an aggressive
    /// ceiling, both survive while bulk tool results around
    /// them evict. The trailing message is also always
    /// pinned (the model must see what just happened).
    #[tokio::test]
    async fn last_user_and_assistant_pinned_even_with_zero_tail_budget() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let context = vec![
            user("the actual directive", 1),
            assistant(&format!("important conclusion {}", "x".repeat(200)), 2),
            tool_result("c1", "bash", &"big output ".repeat(100), 3),
            tool_result("c2", "bash", &"more output ".repeat(100), 4),
        ];
        let policy =
            ContextVirtualizationPolicy::new(5, 0, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ts: Vec<u64> = survivors
            .iter()
            .filter(|m| !is_ledger(m))
            .map(message_timestamp)
            .collect();
        assert!(ts.contains(&1), "latest user pinned: {survivors:?}");
        assert!(ts.contains(&2), "latest assistant pinned: {survivors:?}");
        assert!(ts.contains(&4), "trailing message pinned: {survivors:?}");
        assert!(
            !ts.contains(&3),
            "the older tool result evicts: {survivors:?}"
        );
    }

    /// rlm2/PR5 size-aware eviction: among evictable
    /// messages, an old LARGE tool result (> 1_024 estimated
    /// tokens) evicts before older-but-small assistant
    /// texts. Pure FIFO would have evicted the small
    /// narrative note first for negligible gain.
    #[tokio::test]
    async fn large_old_tool_results_evict_before_small_text() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let big = "z".repeat(5_000); // ~1_250 tokens, over the 1_024 threshold
        let context = vec![
            assistant("narrative note alpha", 1), // oldest AND small
            tool_result("c1", "bash", &big, 2),   // old + large
            assistant("narrative note beta", 3),
            user("current question", 4),
        ];
        // Total ≈ 1_260; ceiling 600 → target 360: evicting
        // the large tool result alone reaches it.
        let policy =
            ContextVirtualizationPolicy::new(600, 30, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ts: Vec<u64> = survivors
            .iter()
            .filter(|m| !is_ledger(m))
            .map(message_timestamp)
            .collect();
        assert!(
            !ts.contains(&2),
            "the large tool result must evict first: {survivors:?}"
        );
        assert!(
            ts.contains(&1),
            "the older small text survives — FIFO would have dropped it: {survivors:?}"
        );
        assert_eq!(ts, vec![1, 3, 4]);
    }

    // ----- rlm2 review fix: pair-atomic eviction -----

    /// No surviving assistant `ToolCall` may lack its result, and
    /// no surviving tool result may lack its call — OpenAI-family
    /// providers reject either orphan with a 400.
    fn assert_tool_pairs_intact(survivors: &[Message]) {
        let mut call_ids: Vec<&str> = Vec::new();
        let mut result_ids: Vec<&str> = Vec::new();
        for m in survivors {
            match m {
                Message::Assistant(a) => {
                    for block in &a.content {
                        if let ContentBlock::ToolCall(c) = block {
                            call_ids.push(c.id.as_str());
                        }
                    }
                }
                Message::ToolResult(t) => result_ids.push(t.tool_call_id.as_str()),
                _ => {}
            }
        }
        call_ids.sort_unstable();
        result_ids.sort_unstable();
        assert_eq!(
            call_ids, result_ids,
            "eviction orphaned a tool_call/tool_result pair: {survivors:?}"
        );
    }

    /// The size-aware pass (3a-ter) evicts a LARGE tool result —
    /// the assistant message whose `ToolCall` produced it must go
    /// with it, or the survivors carry an orphaned call.
    #[tokio::test]
    async fn size_aware_eviction_takes_the_assistant_call_with_its_large_result() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let big = "z".repeat(5_000); // ~1_250 tokens, over the 1_024 threshold
        let context = vec![
            user("do the thing", 1),
            assistant_call_with_id("call_2", "bash", serde_json::json!({"command": "ls"}), 2),
            tool_result("call_2", "bash", &big, 3),
            assistant_call_with_id("call_4", "bash", serde_json::json!({"command": "pwd"}), 4),
            tool_result("call_4", "bash", "pwd output", 5),
            assistant("narrative", 6),
            user("now summarize", 7),
        ];
        let policy =
            ContextVirtualizationPolicy::new(600, 30, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ts: Vec<u64> = survivors
            .iter()
            .filter(|m| !is_ledger(m))
            .map(message_timestamp)
            .collect();
        assert!(!ts.contains(&3), "the large result evicts: {survivors:?}");
        assert!(
            !ts.contains(&2),
            "its assistant call must evict with it: {survivors:?}"
        );
        assert_tool_pairs_intact(&survivors);
    }

    /// The latest-assistant anchor pins a message carrying a
    /// `ToolCall` — its result is part of the same protocol unit,
    /// so the pin protects the whole pair instead of letting FIFO
    /// evict the result out from under the call.
    #[tokio::test]
    async fn pinned_assistant_anchor_protects_its_tool_results_from_eviction() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let filler = "f".repeat(2_000); // ~500 tokens of evictable bulk
        // The result is ~100 tokens — big enough that eviction is
        // still over target after the filler goes, so a per-message
        // FIFO would reach it (skipping only the pinned assistant)
        // and orphan the call.
        let context = vec![
            user(&filler, 1),
            assistant_call_with_id("call_2", "bash", serde_json::json!({"command": "ls"}), 2),
            tool_result("call_2", "bash", &"ls output ".repeat(40), 3),
            user("current directive", 4),
        ];
        // Ceiling far below the filler; pin tail covers only the
        // trailing directive, so ts 2 + 3 are protected purely by
        // the latest-assistant anchor extending over its pair.
        let policy =
            ContextVirtualizationPolicy::new(50, 1, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ts: Vec<u64> = survivors
            .iter()
            .filter(|m| !is_ledger(m))
            .map(message_timestamp)
            .collect();
        assert!(!ts.contains(&1), "the filler evicts: {survivors:?}");
        assert!(ts.contains(&2), "latest assistant pinned: {survivors:?}");
        assert!(
            ts.contains(&3),
            "the pinned call's result must survive with it: {survivors:?}"
        );
        assert_tool_pairs_intact(&survivors);
    }

    /// Regression for the orphaned-pair finding: a low-water batch
    /// eviction sweeping a transcript dense with pairs (where the
    /// old per-message FIFO routinely broke between a call and its
    /// result) leaves no orphan on either side.
    #[tokio::test]
    async fn low_water_batch_eviction_never_orphans_tool_pairs() {
        let store = Arc::new(RwLock::new(ExternalContext::new()));
        let body = "b".repeat(120); // ~30 tokens per result
        let mut context = vec![user("kick off the build", 1)];
        for i in 0..8u64 {
            let ts = 10 + i * 2;
            let id = format!("call_{ts}");
            context.push(assistant_call_with_id(
                &id,
                "bash",
                serde_json::json!({"command": format!("step {i}")}),
                ts,
            ));
            context.push(tool_result(&id, "bash", &body, ts + 1));
        }
        context.push(user("how did it go?", 100));
        let policy =
            ContextVirtualizationPolicy::new(120, 40, 0, store, shared_pushed(HashSet::new()))
                .with_evict_low_water_pct(0.6);
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        assert!(
            survivors.len() < context.len(),
            "the batch eviction must actually fire: {survivors:?}"
        );
        assert_tool_pairs_intact(&survivors);
    }

    /// rlm2/PR5 ledger diet: an identity entry whose result
    /// body is still in the working set is skipped — the
    /// model can see the body, so pointing at the archive is
    /// redundant. An entry whose body was evicted stays
    /// listed.
    #[tokio::test]
    async fn ledger_skips_entries_present_in_working_set() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            assistant_with_tool_call("bash", serde_json::json!({"command": "ls"}), 1),
            tool_result("call_1", "bash", "ls output", 2),
            assistant_with_tool_call("bash", serde_json::json!({"command": "pwd"}), 3),
            tool_result("call_3", "bash", "pwd output", 4),
        ])));
        let pushed: HashSet<u64> = (1..=4).collect();
        // The `ls` result (ts 2) is still in the working set;
        // the `pwd` result (ts 4) lives only in the archive.
        let context = vec![
            user("a question", 100),
            tool_result("call_1", "bash", "ls output", 2),
        ];
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(pushed));
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let ledger_text = user_text(survivors.last().expect("non-empty")).expect("ledger text");
        assert!(
            ledger_text.contains("pwd (id=call_3)"),
            "evicted body stays listed: {ledger_text}"
        );
        assert!(
            !ledger_text.contains("ls (id=call_1)"),
            "body in the working set must not be listed: {ledger_text}"
        );
    }

    /// rlm2/PR5 ledger diet: an identity entry whose result
    /// body was recalled into the sticky archive-recall
    /// message is skipped for the same reason — the body is
    /// already on screen.
    #[tokio::test]
    async fn ledger_skips_entries_recalled_into_sticky_set() {
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(vec![
            assistant_with_tool_call(
                "bash",
                serde_json::json!({"command": "echo relevant_keyword"}),
                1,
            ),
            tool_result("call_1", "bash", "relevant_keyword output text", 2),
        ])));
        let pushed: HashSet<u64> = (1..=2).collect();
        let policy =
            ContextVirtualizationPolicy::new(10_000, 8, 10_000, store, shared_pushed(pushed));
        let context = vec![user("looking for relevant_keyword", 100)];
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall = recall_text_of(&survivors).expect("the result body pages in");
        assert!(recall.contains("relevant_keyword output text"), "{recall}");
        let ledger_text = user_text(survivors.last().expect("non-empty")).expect("ledger text");
        assert!(
            !ledger_text.contains("(id=call_1)"),
            "sticky-recalled body must not be listed in the ledger: {ledger_text}"
        );
    }

    /// rlm2/PR5 × PR3: the per-tool cap and overflow line are
    /// deterministic, so an over-the-cap archive still
    /// renders byte-identical ledgers across appending turns
    /// — the no-op fast path keeps working.
    #[tokio::test]
    async fn capped_ledger_stays_byte_stable_across_appending_turns() {
        let archived: Vec<Message> = (0..10)
            .map(|i| {
                assistant_with_tool_call(
                    "bash",
                    serde_json::json!({ "command": format!("cmd{i}") }),
                    i as u64 + 1,
                )
            })
            .collect();
        let pushed: HashSet<u64> = (1..=10).collect();
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(archived)));
        let policy = ContextVirtualizationPolicy::new(10_000, 8, 0, store, shared_pushed(pushed));

        let context = vec![user("question", 100), assistant("working on it", 101)];
        let sent_1 = apply_response(policy.before_model(sample_request(&context)).await, context);
        let ledger_text = user_text(sent_1.last().expect("non-empty")).expect("ledger text");
        assert!(ledger_text.contains("cmd9"), "{ledger_text}");
        assert!(!ledger_text.contains("cmd1 "), "{ledger_text}");
        assert!(ledger_text.contains("2 earlier calls"), "{ledger_text}");

        // A plain text append re-renders the capped lines
        // byte-identically → Continue.
        let mut context_2 = sent_1;
        context_2.push(assistant("a plain text reply", 102));
        let response = policy.before_model(sample_request(&context_2)).await;
        assert_eq!(response, BeforeModelResponse::Continue);
    }

    /// rlm2/PR5 (perf): candidate scoring runs against the
    /// token sets cached at archive time and borrows every
    /// candidate from the store — `RelevanceCandidate<'_>`'s
    /// lifetime makes cloning unselected bodies structurally
    /// impossible; the only owned output is the rendered
    /// section text of the items that fit the budget. This
    /// test pins the observable contract: with one matching
    /// candidate among many large irrelevant bodies, exactly
    /// one section's worth of text is produced, and the
    /// archive entries carry their pre-computed token sets.
    #[tokio::test]
    async fn candidate_scoring_does_not_clone_unselected_bodies() {
        let filler = format!("unrelated filler {}", "lorem ipsum dolor ".repeat(50));
        let mut archived: Vec<Message> = (0..30).map(|i| user(&filler, i as u64 + 1)).collect();
        archived.push(user("the relevant_keyword note", 50));
        let pushed: HashSet<u64> = archived.iter().map(message_timestamp).collect();
        let store = Arc::new(RwLock::new(ExternalContext::from_messages(archived)));
        {
            let external = store.try_read().expect("uncontended");
            assert!(
                external.iter_stored().all(|s| !s.tokens.is_empty()),
                "every archived body has its token set cached at push time"
            );
        }
        let policy = ContextVirtualizationPolicy::new(
            10_000,
            8,
            10_000,
            Arc::clone(&store),
            shared_pushed(pushed),
        );
        let context = vec![user("looking for relevant_keyword", 100)];
        let survivors = match policy.before_model(sample_request(&context)).await {
            BeforeModelResponse::ReplaceMessages(s) => s,
            other => panic!("expected ReplaceMessages, got {other:?}"),
        };
        let recall = recall_text_of(&survivors).expect("the matching note pages in");
        assert_eq!(
            recall.matches("[archive entry").count(),
            1,
            "only the selected candidate renders a section: {recall}"
        );
        assert!(recall.contains("the relevant_keyword note"), "{recall}");
    }
}
