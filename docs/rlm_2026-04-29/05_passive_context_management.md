# Plan 05 — Passive context management

**Branch:** TBD.
**Status:** complementary to RLM, tracked here so it isn't
lost. Worth doing in parallel with Plan 02 if eval data
shows context pressure within a turn is the limiting
factor.

## Rationale

Compaction (current state) is *reactive*: the gate fires
when context already exceeds the threshold. The model
sees a sudden context shift mid-turn that may or may not
preserve the information it cared about.

RLM (Plan 02) is *opt-in*: the model has to choose to
recurse rather than read everything. Models that don't yet
know to use `recurse` keep filling their context the old
way.

Passive context management is the third leg: the harness
manages what's in the context window without either
waiting for overflow or requiring the model to act. Two
flavors, both compelling, neither implemented:

**(a) Background summarization.** A separate task watches
the in-flight context as it grows. When it predicts the
*next* turn will trip the threshold, it summarizes the
oldest stable region and queues a `BeforeModelPolicy`
action to swap that region in on the next turn. From the
model's perspective the swap is silent — the context just
didn't grow as much as it would have.

**(b) Just-in-time relevance filtering.** Before each
model turn, `BeforeModelPolicy` scores each message for
relevance to the current request and drops the bottom
quartile. A reranker (cheap embedding similarity, or even
keyword overlap) provides the score. The model sees a
smaller, denser context tuned to what it's working on.

(a) is closer to existing compaction; (b) is closer to RLM
in spirit — it filters rather than compresses.

## Why both can co-exist

The two flavors operate on different parts of the run:

- **(a) Background summarization** manages the **oldest**
  messages in the active turn's context. Compresses
  history that's decided and stable.
- **(b) Just-in-time filtering** manages the **whole** active
  context based on the current request's relevance.
  Drops noise.
- **Plan 01 stagnation-aware compaction** is the safety net
  when neither (a) nor (b) is enough.
- **Plan 02 RLM `recurse`** is the escape hatch for
  context that *would* have been useful but doesn't fit.

A run could use all four: `recurse` to navigate huge
external corpora, JIT filtering to keep the active context
relevant, background summarization to compress old stable
parts, and stagnation-aware compaction as the fallback.

## Design — flavor (a): background summarization

### State

The controller spawns a `BackgroundSummarizer` task at
run-start that holds:

```rust
struct BackgroundSummarizer {
    /// Read-only handle to the current run's context. Updated
    /// by the controller at each REPL boundary.
    context_view: Arc<RwLock<Vec<Message>>>,
    /// Pre-built summaries indexed by the message-range they
    /// cover. Consumed by BeforeModelPolicy on the next turn.
    pending_summary: Arc<Mutex<Option<PreBuiltSummary>>>,
    summarizer: Arc<dyn MessageSummarizer>,
    config: BackgroundSummarizerConfig,
}

struct PreBuiltSummary {
    range: std::ops::Range<usize>,  // indexes into context
    summary: String,
    tokens_replaced: u64,
}

struct BackgroundSummarizerConfig {
    /// Predict that the next turn will overflow if the
    /// current context is at this fraction of threshold.
    forecast_threshold: f64,  // e.g. 0.85
    /// Don't summarize anything within this many tokens of
    /// the most-recent message — those are still in-flight.
    keep_recent_tokens: u64,
}
```

### Flow

1. After each `Print` step in the REPL loop, the controller
   sends a "context updated" notify to the background task.
2. The background task wakes up, reads the new context
   (via `RwLock::read`), and decides:
   - Is the current context above `forecast_threshold *
     (window - reserve)`?
   - If yes: take the prefix above `keep_recent_tokens`
     from the end, summarize it, store the summary in
     `pending_summary`.
3. The next REPL iteration's `BeforeModelPolicy` consults
   `pending_summary`. If present and still applicable
   (the indexed range hasn't been re-shaped by user
   intervention), the policy swaps the indexed range out of
   context and the summary in via
   `BeforeModelResponse::ReplaceMessages` — a new variant
   needed for this plan.

### New `BeforeModelResponse` variant

```rust
enum BeforeModelResponse {
    Continue,
    AppendMessages(Vec<Message>),
    /// Replace `context[range]` with the supplied messages.
    /// Used for background-summarization swap-ins.
    ReplaceMessages {
        range: std::ops::Range<usize>,
        replacement: Vec<Message>,
    },
}
```

This is a real protocol change to the `BeforeModelPolicy`
contract introduced in PR 7 of the REPL refactor. PR 1's
characterization tests don't cover this surface so it
shouldn't break them, but the policy-test (Plan 07 of REPL)
needs an additional case.

### Why this beats reactive compaction

- **Latency-masked.** The summarization happens in
  parallel with model inference, not blocking the user.
  When the swap-in happens, the user perceives no pause.
- **Predictive.** Compaction reacts to "we're already
  over"; this reacts to "we're about to be over." The
  summarizer has more breathing room to do good work.
- **Stable.** The summarized region is the oldest stable
  prefix — the part that's least likely to be re-read.

### Risks

- **Predicate accuracy.** If the forecaster is too
  aggressive, we summarize when we don't need to (cost
  without benefit). If too lax, we miss the window. The
  `forecast_threshold` knob tunes this; eval data would
  drive the right value.
- **Race with user intervention.** If the user types a
  prompt that interrupts the active run between
  "background summary built" and "policy swaps it in," the
  summary's index range may be stale. The policy must
  re-validate (or just bail) when the range no longer
  matches the expected message kinds.

## Design — flavor (b): just-in-time relevance filtering

### Reranker choice

Three plausible scoring strategies, in increasing cost /
quality:

1. **Keyword overlap.** Tokenize the user's most recent
   prompt; score each candidate message by how many tokens
   match. Cheap, no extra dependencies, surprisingly
   effective for code-search-flavored requests.
2. **TF-IDF over the run's history.** Slightly better than
   raw overlap; introduces a small in-memory index. Pure
   Rust, no model.
3. **Embedding similarity** via a local embedding model
   (Ollama's `nomic-embed-text` is a common choice).
   Highest quality, requires a second model load.

Start with (1). The rerank cost is single-digit
milliseconds for a typical run; it can be re-evaluated on
every turn without budget concerns.

### Policy implementation

```rust
struct JitRelevancePolicy {
    /// Drop messages scoring below this threshold (0..=1.0).
    drop_threshold: f64,
    /// Always keep the last N messages regardless of score.
    keep_recent_count: usize,
    /// Always keep messages of these kinds (e.g., the system
    /// prompt and current user prompt).
    keep_kinds: Vec<MessageKind>,
}

#[async_trait]
impl BeforeModelPolicy for JitRelevancePolicy {
    async fn before_model(&self, request: BeforeModelRequest<'_>)
        -> BeforeModelResponse {
        let scored = score_messages(request.context, &most_recent_user_prompt);
        let to_drop = scored
            .iter()
            .filter(|s| s.score < self.drop_threshold)
            .filter(|s| !self.is_pinned(s))
            .map(|s| s.index)
            .collect::<Vec<_>>();
        if to_drop.is_empty() {
            return BeforeModelResponse::Continue;
        }
        BeforeModelResponse::DropMessages { indexes: to_drop }
    }
}
```

This needs another `BeforeModelResponse` variant:

```rust
enum BeforeModelResponse {
    Continue,
    AppendMessages(Vec<Message>),
    ReplaceMessages { range: ..., replacement: ... },
    DropMessages { indexes: Vec<usize> },
}
```

### Why this is more aggressive than (a)

(a) preserves information by summarizing it; (b) discards
it. That's a sharper trade-off — if the score is wrong, the
model loses real context. Pinning rules (system prompt,
last N messages, current prompt) mitigate this.

### Risks

- **Score quality.** Keyword overlap on the *most recent*
  user prompt is a narrow signal. A user changing topic
  mid-run has the prompt change too — old context that's
  about to be relevant again could get dropped right
  before they ask about it.
- **Information loss is silent.** Unlike summarization,
  dropped messages aren't replaced with anything. If the
  model wonders "did we already do X?" the answer disappears.
  Mitigation: surface a system-message footnote
  ("dropped 3 older messages by relevance") so the model
  knows.

## When to do which

If eval data after Plan 02 shows:

- **The model uses `recurse` effectively but turns are
  still slow because of context swelling within a turn**:
  ship flavor (a). The masked-latency win is real for
  long-running multi-tool turns.
- **The model under-uses `recurse` and context
  accumulation is the bottleneck**: ship flavor (b). It
  gets information out of the way without requiring the
  model to act.
- **Both are true**: ship (a) first, (b) second. (a) is
  lower-risk because it preserves information; (b) is the
  bigger win when it works.
- **Neither**: probably means Plan 02 is doing the heavy
  lifting and these aren't worth the maintenance burden.

## Files (sketch)

For flavor (a):

- `crates/anie-cli/src/background_summarizer.rs` — new
  file, the `BackgroundSummarizer` struct + spawn logic.
- `crates/anie-cli/src/controller.rs` — wire spawn at
  run-start, send notifies on each REPL boundary.
- `crates/anie-agent/src/agent_loop.rs` — add
  `ReplaceMessages` to `BeforeModelResponse`.
- New tests.

For flavor (b):

- `crates/anie-tools/src/rerank.rs` (or in `anie-agent`) —
  the keyword-overlap scorer.
- `crates/anie-cli/src/jit_relevance.rs` — the policy
  implementation.
- `crates/anie-agent/src/agent_loop.rs` — add
  `DropMessages` to `BeforeModelResponse`.
- New tests.

## Risks (cross-cutting)

- **The protocol additions to `BeforeModelResponse` are
  permanent.** Each new variant constrains what the
  default-noop policy and existing implementations need to
  handle. Add only the one we're shipping; keep the other
  variant in the plan but not the code.
- **Both flavors need the eval suite to pick the right
  knobs.** Without it we can't tell if a `forecast_threshold`
  of 0.85 is too aggressive or too timid.

## Exit criteria

Per-flavor; to be filled in when the plan is unblocked.

## Deferred

- Combined (a) + (b) coordination. If both are running,
  they need to agree on which messages they're handling.
  Out of scope for first ship of either.
- Tunable knobs as user-facing config. Initial ships use
  hard-coded defaults; expose later if eval data shows
  variability matters.
