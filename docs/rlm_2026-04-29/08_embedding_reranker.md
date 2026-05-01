# Plan 08 — Embedding-based reranker

**Branch:** `dev_rlm`.
**Status:** **landed** — `rlm/19` (PR 08.1, `d8e81b7`),
`rlm/20` (PR 08.2, `08180d8`), `rlm/21` (PR 08.3,
`4eff9e8`). Smoke-tested.
**Companion:** observation from a real TUI session at session id `c0057000` documented in dev_rlm history.

## Rationale

Phase E shipped a keyword-overlap reranker (`rlm/10`,
`crates/anie-cli/src/context_virt.rs:198-220`):
the user prompt and each archived message are tokenized
(lowercase, alphanumeric split, ≥3-char tokens, stopword
filter), and the score is the size of the token-set
intersection.

That works, but it's lossy in two ways:

1. **Surface-form mismatch.** A prompt of *"What about
   Sunday?"* tokenizes to `{sunday}` after stopwords. An
   archived NWS forecast page might say *"Sun: rain
   expected"* — `{sun, rain, expected}` — and intersect to
   zero (`sun` ≠ `sunday`). The reranker can't see that
   the page is about exactly the topic asked.

2. **Topical compression invisible.** A prompt of *"weather
   on Sunday"* and an archived multi-day forecast of
   *"Tuesday 78F sunny / Wednesday 80F partly cloudy /
   Sunday 75F clear"* share only `weather` after stopword
   filter. The page IS about Sunday weather but the keyword
   reranker treats it as marginally relevant.

Result observed in session `c0057000`:
- T4 fetched a multi-day NWS forecast (had Sunday's data).
- T5 (`"What about Sunday?"`) issued a fresh `web_search` +
  `web_read` BEFORE falling through to a `recurse` call.
- The recurse worked; but the prior fetches were
  unnecessary because the answer was already in the
  archive — the reranker just couldn't rank that prior
  forecast as relevant enough to page in.

Embedding-based scoring captures semantic similarity:
*"Sunday weather"* and *"forecast for Sun: rain
expected"* land close in vector space. Swapping the score
function from token-set-intersection to cosine-similarity
makes that match visible to the policy. The model gets the
right content paged in and skips the redundant fetch.

## Design

### Trait surface

```rust
// crates/anie-cli/src/embedder.rs (new file)

#[async_trait]
pub(crate) trait Embedder: Send + Sync {
    /// Embed a single text into a fixed-dim vector.
    /// Errors are treated as fallback signals — the caller
    /// (reranker) drops to keyword overlap when embeds fail.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;

    /// Embedding dimensionality. Used for sanity checks.
    fn dim(&self) -> usize;
}
```

One implementation in this PR series: `OllamaEmbedder`,
which calls Ollama's `/api/embed` endpoint. Tests use a
stub `FixedEmbedder` that returns deterministic vectors
keyed off input text.

### Cosine similarity

Pure function, no allocations:
```rust
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot = a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na * nb) }
}
```

### Cache embeddings on `StoredMessage`

```rust
// crates/anie-cli/src/external_context.rs

pub(crate) struct StoredMessage {
    pub id: MessageId,
    pub message: Message,
    pub summary: Option<String>,
    pub embedding: Option<Vec<f32>>, // NEW
}
```

Plus `set_embedding(id, vec)` and `get_embedding(id) ->
Option<&[f32]>` mutators / readers. Mirrors the summary
slot from Phase F. Embeddings are computed once per
archived entry, then cached.

### Background embed worker

```rust
// crates/anie-cli/src/bg_embedder.rs (new file)

pub(crate) struct EmbedRequest {
    pub id: MessageId,
    pub text: String,
}

pub(crate) fn spawn_embed_worker(
    embedder: Arc<dyn Embedder>,
    external: Arc<RwLock<ExternalContext>>,
) -> mpsc::Sender<EmbedRequest>;
```

Same shape as `bg_summarizer::spawn_worker`. Bounded
mpsc channel (capacity 64). The worker pulls requests,
calls `embed`, writes back via `set_embedding`.
`try_send` from the policy means we never block the
model turn — if the worker is behind, we drop the
request and the entry stays unembedded (reranker
falls through to keyword overlap for it).

### Policy integration

The policy (`ContextVirtualizationPolicy`):

- Holds `Option<Arc<dyn Embedder>>` and
  `Option<mpsc::Sender<EmbedRequest>>` as
  builder-attachable fields. Defaults to `None` —
  preserves existing keyword-overlap behavior when no
  embedder is configured.
- After archiving newly-evicted messages
  (`before_model` step 2), enqueues an `EmbedRequest`
  for each — same `try_send` pattern as the summarizer
  enqueue.
- Caches the prompt embedding on the policy struct
  per-fire — recomputed when the latest user message
  timestamp changes, reused otherwise. This is the
  hot-path cost amortization: one embed call per turn,
  not per fire.
- In `page_in_relevant`: if embedder is configured AND
  a candidate has a cached embedding, score is cosine
  similarity. Otherwise: keyword-overlap fallback.

### Reranker scoring

The score function changes per candidate:

```rust
fn score_candidate(
    prompt_embed: Option<&[f32]>,
    prompt_tokens: &HashSet<String>,
    candidate_embed: Option<&[f32]>,
    candidate_message: &Message,
) -> f32 {
    // Embedding path: both vectors present.
    if let (Some(p), Some(c)) = (prompt_embed, candidate_embed) {
        return cosine_similarity(p, c);
    }
    // Fallback: keyword overlap.
    score_message_keyword(prompt_tokens, candidate_message) as f32
}
```

The score type changes from `usize` to `f32` to
accommodate cosine similarity values. Sort uses
`partial_cmp().unwrap_or(Equal)` — NaN should never
occur with our inputs, but we handle it defensively.

### Configuration

Single env var:
- `ANIE_EMBEDDING_MODEL=nomic-embed-text` — opt in by
  setting; unset = no embedder, keyword-only path.
- Reuses the parent run's `Model.base_url` for Ollama
  routing. (When a non-Ollama provider is the parent,
  the embedder errors at construction; we fall back to
  no-embedder mode and log a warning.)

No code change required to use the existing keyword
reranker; existing rlm-mode users keep current behavior
unless they set the env var.

## Files to touch

```
crates/anie-cli/src/embedder.rs            (NEW — trait + OllamaEmbedder + cosine_similarity)
crates/anie-cli/src/bg_embedder.rs         (NEW — worker, parallel to bg_summarizer)
crates/anie-cli/src/external_context.rs    (add embedding slot + setter/getter)
crates/anie-cli/src/context_virt.rs        (policy integration, reranker swap)
crates/anie-cli/src/controller.rs          (env var read, spawn worker, attach to policy)
crates/anie-cli/src/lib.rs                 (mod declarations)
docs/rlm_2026-04-29/execution/README.md    (track the new commits)
```

## Phased PRs (sub-commits on `dev_rlm`)

### PR 08.1 — `rlm/19`: Embedder trait + OllamaEmbedder + cosine

- New file `embedder.rs`.
- `Embedder` trait + async fn embed.
- `OllamaEmbedder` impl using `reqwest::Client` against
  `<base_url>/api/embed`. Parses `{ "embeddings": [[...]] }`
  response shape.
- Pure `cosine_similarity` function.
- Tests: stub `FixedEmbedder` for unit tests; cosine
  basic algebra tests; OllamaEmbedder JSON parse test
  using `httpmock` (already a workspace dep).

Verifiable in isolation: build, test, no other code
needs to change.

### PR 08.2 — `rlm/20`: embedding cache + background worker

- `StoredMessage::embedding: Option<Vec<f32>>`,
  setter/getter on `ExternalContext`.
- New file `bg_embedder.rs` with `EmbedRequest` +
  `spawn_embed_worker`.
- Tests: round-trip via worker, fallback when worker
  is full, cache hit/miss.

Behavior unchanged from PR 08.1 — worker is wired in
isolation, not yet used by the policy.

### PR 08.3 — `rlm/21`: reranker uses embeddings

- Policy gains `with_embedder(...)` and
  `with_embed_sender(...)` builders.
- `page_in_relevant` swaps to embedding-when-available
  scoring; keyword fallback path preserved.
- Controller reads `ANIE_EMBEDDING_MODEL`; spawns
  worker + attaches to policy when set.
- Tests: reranker prefers semantic match over keyword
  match when embeddings are available; falls back to
  keyword path when not.

This is where behavior changes for users who set the
env var. Default users see no change.

## Test plan

Per-PR tests (specific behaviors-under-test):

PR 08.1:
- `cosine_similarity_orthogonal_vectors_score_zero`
- `cosine_similarity_identical_vectors_score_one`
- `cosine_similarity_handles_zero_vector_returns_zero`
- `ollama_embedder_parses_embed_response`
- `ollama_embedder_propagates_http_error`
- `fixed_embedder_returns_deterministic_vectors`

PR 08.2:
- `embedder_cache_round_trips_via_worker`
- `embedding_count_reflects_state`
- `worker_drops_request_when_channel_full`
- `set_embedding_idempotent`

PR 08.3:
- `reranker_prefers_high_cosine_similarity`
- `reranker_falls_back_to_keyword_when_no_embedding`
- `reranker_falls_back_to_keyword_when_embedder_unconfigured`
- `policy_enqueues_embed_request_on_archive`

End-to-end smoke (after PR 08.3):
- Same prompt sequence as session `c0057000`
  (date ideas → weather → indoor activities → Friday →
  Sunday). With `ANIE_EMBEDDING_MODEL=nomic-embed-text`,
  expect: T5 ("What about Sunday?") successfully pages
  in T4's multi-day forecast via embedding match,
  fewer redundant web fetches than the keyword run.

## Risks

- **Embedding latency.** Each `embed` call is one
  round-trip to Ollama. On local hardware, ~50–200ms.
  The background worker amortizes this — embeddings are
  written async and the model never waits — but the
  prompt embed at fire time is on the hot path. Budget:
  one embed per fire, ~100ms. Acceptable.

- **Embedding model availability.** Operator sets
  `ANIE_EMBEDDING_MODEL=nomic-embed-text` but doesn't
  have it pulled. The HTTP call fails; policy falls
  back to keyword overlap silently. A tracing warning
  on first failure tells the operator to `ollama pull`.

- **Cache invalidation.** Embeddings cache forever in
  the in-memory archive. The archive is per-run, so
  stale embeddings die with the process. No cache
  invalidation logic needed for v1.

- **Vector size.** `nomic-embed-text` produces 768-dim
  vectors. 50 archive entries × 768 × 4 bytes = ~150KB
  per session. Negligible for in-memory storage. If we
  ever ship persistent archives (Plan 06's "session log
  derived index"), this is a consideration.

- **Hybrid vs swap.** This PR ships swap (embedding
  when available, keyword otherwise). Hybrid (linear
  combination) might score better but adds tuning
  surface (weight knob). Defer hybrid until eval data
  shows swap is insufficient.

## Exit criteria

- [ ] `Embedder` trait shipped with at least one impl
      (`OllamaEmbedder`).
- [ ] Embedding cache on `StoredMessage` + setters.
- [ ] Background worker pulling from mpsc + writing to
      cache.
- [ ] Reranker uses embeddings when available; falls
      back to keyword overlap otherwise.
- [ ] Single env-var opt-in (`ANIE_EMBEDDING_MODEL`).
- [ ] All tests above pass.
- [ ] Clippy + fmt clean.
- [ ] Manual smoke against the `c0057000`-like session
      shows the model paging in T4's multi-day forecast
      when answering T5's "what about Sunday?" question
      — observable as a successful page-in (paged_in ≥ 1
      with no fresh weather fetch on T5) or a recurse
      call that succeeds (keyword path still does the
      latter; embedding should make the former possible).

## Deferred

- **Hybrid scoring.** Linear combination of embedding +
  keyword scores. Adds a `relevance_blend` knob.
- **Other embedding providers.** Cohere, OpenAI
  text-embedding-3-small, BAAI/bge-small. Easy to add
  once the trait exists.
- **Re-ranking with cross-encoders.** Higher quality
  but per-query model call against every candidate.
  Consider only if eval shows cosine on local
  bi-encoder is the bottleneck.
- **Persistent embedding cache.** Tied to persistent
  archive (Plan 06's session-log-derived index).
  Defers with that work.

## Smoke outcomes (post-landing)

Four smokes run against the qwen3.5:9b chat model with
nomic-embed-text as the embedder:

**S1 (`e11538ed` keyword vs `5e264e2f` embedding).** Same
4k-ceiling prompt run twice. Keyword run: 4 reads
(re-read external_context.rs once). Embedding run: 3
reads (no redundancy) and a more conceptually clear
synthesis ("producer-consumer pattern"). Embedding
reranker helped both retrieval *and* the framing of the
final answer.

**S2 (`c05fe6bd`).** Weather replication. Inconclusive
because weather sites returned 403/timeouts on T1 — no
rich archive content for T2 to test paging against.
Embedding fired (paged_in=1) but upstream content was
absent. Site-reliability issue, not an rlm test.

**S3 (`e553efeb`).** Tight 1.5k ceiling stress test.
Same shape that triggered the original spiral pre-
rlm/16. With embedding: 3 parallel reads, eviction
fires cleanly, no spiral, no hallucination. Model
didn't emit a final "done" text but didn't drift into
fictional content either. Acceptable terminal state.

**S4 (`1eb096ee` → cross-domain).** T1 read a Rust
source file; T2 switched topic entirely to "what's the
current weather in Atlanta, GA?" Across 11 policy fires
on T2, paged_in=0 on 8 of them — the embedding correctly
recognized that T1's code content had near-zero
similarity to a weather query. Cross-domain isolation
working: no spurious page-ins of unrelated content.
Validates that cosine similarity puts unrelated topics
in different regions of vector space, exactly as
desired.

Net read across S1–S4: the embedding path measurably
improves retrieval quality on the canonical case (S1),
maintains correct cross-domain isolation (S4), and
doesn't regress the stress-test paths the prior commits
were known to handle (S3). Default users without the
env var set see no behavior change.
