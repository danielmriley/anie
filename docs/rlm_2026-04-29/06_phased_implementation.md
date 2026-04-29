# Plan 06 — Phased path from `recurse` tool to full context virtualization

**Branch:** `dev_rlm` (after Plan 01 lands on `main`).
**Status:** ready to spec; supersedes the standalone framing
in Plans 02 and 05 by ordering them as one engineering
sequence.

## Rationale

The unified picture we want — articulated after writing Plans
02–05 — is **context virtualization**: the harness keeps the
model's active context small (a fixed ceiling well below the
model's actual window), and content that exceeds the ceiling
gets paged out to an indexed external store. The model
navigates the external store via the `recurse` tool. Old
content doesn't bloat new turns; the model performs at its
quality ceiling on every call.

This is an inversion of the current model. Today the
**model** owns its context (everything it sees is in the
window); the harness only intervenes when overflow triggers
compaction. With virtualization the **harness** owns the
context; the model is a guest with read-on-demand access.

The whole vision is reachable inside Shape 1 (recurse as a
tool) plus the existing `BeforeModelPolicy` hook from PR 7 of
the REPL refactor. No new agent-loop intent variants. No
custom-trained model. The work is entirely in `anie-tools`
(the recurse tool) and `anie-cli` (the policy that owns active
context).

## Phases

Each phase ships independently and is reviewable on its own
diff. Each phase produces a measurable artifact the eval
suite (Plan 07) can score.

### Phase A — `recurse` tool (Plan 02 as written)

The tool the model calls. Validates that recursive sub-calls
work at all. Without this, none of the later phases have
anywhere to send the model when it needs paged-out content.

**Effort:** ~1 week.
**Surface:** new tool in `anie-tools-web` or a new
`anie-tools-recurse` crate. New `SubAgentFactory` and
`ContextProvider` traits in `anie-agent`.
**Exit:** Plan 02's exit criteria. Smoke against
qwen3.5:9b shows the model successfully recursing into a
prior turn to answer a follow-up question.

### Phase B — Indexed external store

Adds the data structure. Without it, "external context" is
just a vague pointer to the parent run's full context — fine
for Phase A's tool, but inadequate for Phase C's eviction
work.

The store needs:

- **Addressable storage.** Each evicted message gets a stable
  ID; the recurse tool resolves IDs to content.
- **Lightweight indexing.** Filter by message kind (User /
  Assistant / ToolResult / Custom), by tool name, by recency,
  by free-text keywords (Aho-Corasick is fine to start).
- **Optional persistence.** In-memory for v1; disk-backed
  via the existing session log for v2 (sessions already store
  every message; we just need a derived index).

```rust
// crates/anie-cli/src/external_context.rs (new file)

pub struct ExternalContext {
    /// Messages indexed by stable ID. The ID is the message's
    /// position in the original run's context plus a salt
    /// (so re-evicting a message doesn't collide).
    by_id: HashMap<MessageId, Message>,
    /// Index by message kind for fast scope=kind lookups.
    by_kind: HashMap<MessageKind, Vec<MessageId>>,
    /// Index by tool name for `ToolResult` messages.
    by_tool: HashMap<String, Vec<MessageId>>,
    /// Compact text index for keyword search (Phase E).
    keyword_index: KeywordIndex,
}
```

**Effort:** ~1 week.
**Surface:** new module + types. The recurse tool from Phase
A starts reading from this store instead of the parent's
in-memory context.
**Exit:** Phase A's tests still pass with the store
underneath. Eviction is not yet wired up (the active context
contains everything; the store just mirrors it).

### Phase C — `ContextVirtualizationPolicy`: active context ceiling + FIFO eviction

The first turn-sized piece of the virtualization vision. A
`BeforeModelPolicy` implementation that:

1. Computes the active context's token count using
   `anie_session::estimate_message_tokens`.
2. If above the configured `active_ceiling_tokens` (default:
   16k for small models, configurable), evicts oldest
   messages — except pinned kinds (system note, current user
   prompt, the last N turns) — until under ceiling.
3. Evicted messages move to the `ExternalContext` store from
   Phase B.
4. Returns
   `BeforeModelResponse::ReplaceMessages { range, replacement }`
   where the replacement is the surviving subset (per Plan
   05's variant; needs to land in Phase C).

```rust
pub struct ContextVirtualizationPolicy {
    active_ceiling_tokens: u64,
    /// Always-keep last N turns regardless of ceiling.
    keep_last_turns: usize,
    /// Shared with the recurse tool so evicted content is
    /// readable.
    external: Arc<ExternalContext>,
}
```

**Effort:** ~1 week.
**Surface:** new `BeforeModelPolicy` impl in `anie-cli`. The
new `BeforeModelResponse::ReplaceMessages` variant from Plan
05 (must land here if it hasn't yet).
**Exit:**
- Active context never exceeds `active_ceiling_tokens` at
  any model call.
- Evicted messages are reachable via `recurse(scope:
  message_id, ...)`.
- PR 1's REPL behavior characterization tests pass with the
  policy installed at default `active_ceiling_tokens` =
  effectively unlimited (so default behavior is unchanged).
- A new opt-in test: with `active_ceiling_tokens = 8k`, a run
  that would have accumulated 50k of context still shows the
  model only 8k worth at any given step.

### Phase D — Ledger injection

The model needs to know what's externally available, otherwise
it never reaches for `recurse`. Phase D extends the policy
from Phase C to inject a structured "ledger" system note as
the first or last system message at every turn:

```text
[external context — use the recurse tool to access]
- 47 prior messages (oldest 38 evicted; most recent 9 active)
- 8 tool results: web_read x3, bash x2, grep x1, ls x1, read x1
- 3 files visited via web_read: weather.gov, en.wikipedia.org/wiki/Tallahassee, ...
- 2 prior summaries available
```

The ledger is short (target 200–500 tokens), structured, and
updates on every turn. It replaces what the model would
otherwise infer from looking at the bloated context — except
the inference now is "I have these resources; do I need to
recurse?" rather than "what was that thing 30 turns ago?".

**Effort:** ~3-5 days.
**Surface:** extends `ContextVirtualizationPolicy`; uses
`ExternalContext`'s indexes from Phase B.
**Exit:** the ledger appears at every turn, is bounded in
size, and reflects current external state. Eval scenarios
that require accessing evicted content show the model
choosing to `recurse` rather than guessing.

### Phase E — Smart inclusion (relevance-based paging-in)

Currently active = "what's most recent." Phase E adds: active
= "most recent + most relevant to the current request." A
cheap reranker scores every external message against the
current user prompt, and the top-K (within budget) get paged
back in for this turn.

Reranker options (any of these works; pick by cost):

- **Keyword overlap.** Tokenize the user's prompt; score by
  intersection with each external message's content. Cheap.
- **Embedding similarity.** Local embedding model (Ollama's
  `nomic-embed-text` is the obvious choice). Higher quality,
  one extra round-trip per turn.

Pinning rules apply: the most-recent-N stays active,
regardless of relevance. Eviction is reversible per turn.

**Effort:** ~1 week.
**Surface:** extends `ContextVirtualizationPolicy` with a
scoring step before eviction. Optional Ollama embedding
model integration.
**Exit:** eval scenarios that require content the model
hadn't recently seen show that content showing up in the
active context for the relevant turn (without the model
having to `recurse`).

### Phase F — Background summarization for paged-out content

When content is paged out, summarize it in a background task
and store the summary alongside the original in the
`ExternalContext`. Recurse calls hit summary-first, expand
on demand. This composes with Plan 05a (background
summarization) but specifically for paged-out content rather
than for active-context compression.

**Effort:** ~1 week.
**Surface:** new `BackgroundSummarizer` worker + a `Summary`
field on `ExternalContext` entries.
**Exit:** evicted content has a summary entry within 5
seconds of eviction. Recurse calls that don't need the full
content can read the summary at lower cost.

## Total scope

Sum of phases A–F: ~5–6 weeks of focused work, in
small-PR increments. Each phase ships behind defaults that
preserve current behavior, so we can land them
incrementally without breaking existing usage.

## Composition with shapes 2 and 3

**Shape 2 (recurse-as-intent)** becomes worth doing if eval
data after Phase E shows the model under-uses recurse —
e.g., recurses ~5% of turns when the ledger says it should
be ~30%. Shape 2 lets the harness pre-fetch likely-needed
content via an explicit `Recurse` intent rather than relying
on the model to figure it out. Plan 03's spec already
describes this.

**Shape 3 (native RLM model)** is unblocked the moment a
natively-recursive small model is published to a backend
anie supports. Phase D's ledger format and Phase E's
inclusion policy will need tuning per the model's training
distribution; the rest of the infrastructure carries over.
Plan 04's spec already describes this.

## Why this works inside Shape 1 architecturally

Three load-bearing claims:

1. **The recurse tool is just a tool.** The agent loop's
   tool-execution path doesn't need to know recursion is
   special. The sub-agent runs as a self-contained
   `AgentRunMachine`; the parent gets a `ToolResult` with
   the sub-call's text. No protocol change.
2. **`BeforeModelPolicy` already supports the
   replace/drop pattern.** Plan 05 calls out the variants we
   need (`ReplaceMessages`, `DropMessages`); they're additive
   to an enum that's already shipped. No protocol churn.
3. **The session log gives us free persistence.** Anie
   already writes every message to disk in JSONL. The
   `ExternalContext` store can be a derived index over that
   log; no new persistence layer.

If any of those three turn out to be wrong as we ship, that's
a signal to revisit Shape 2 (intent-based recursion). For now,
the bet is that they hold.

## Files / scope

```
crates/anie-tools/src/recurse.rs         (Phase A — new)
crates/anie-agent/src/lib.rs              (Phase A — SubAgentFactory + ContextProvider traits)
crates/anie-cli/src/external_context.rs   (Phase B — new module)
crates/anie-cli/src/context_virt.rs       (Phase C — new module, the policy)
crates/anie-cli/src/controller.rs         (Phase A/C — install tool + policy)
crates/anie-agent/src/agent_loop.rs       (Phase C — Replace/Drop variants on BeforeModelResponse)
crates/anie-cli/src/system_prompt.rs      (Phase A — SystemPromptKind::SubAgent)
crates/anie-cli/src/ledger.rs             (Phase D — ledger generator, may merge with context_virt)
crates/anie-cli/src/relevance.rs          (Phase E — keyword/embedding reranker)
crates/anie-cli/src/bg_summarizer.rs      (Phase F — background task)
```

## Risks

- **The ledger gets stale.** If it's regenerated every turn
  but the external store changes mid-turn (because Phase F's
  background summarizer wrote a new summary), the model
  might see a ledger that disagrees with `recurse`'s output.
  Mitigation: ledger generation reads a snapshot at turn
  start; mismatches are tolerated.
- **The model doesn't learn to recurse.** Small models need
  examples to use unfamiliar tools. The system prompt for
  Phase A includes one or two recurse examples; if eval data
  shows the model still doesn't reach for it, we go to
  Shape 2 (intent) which removes the choice.
- **Active ceiling is too tight.** A pinned-kinds set that's
  too generous defeats the ceiling; too restrictive and the
  model loses turn continuity. Default is "last 3 turns +
  system + current prompt"; tunable.
- **External-store drift across runs.** The session log is
  permanent; the external store mirrors it. If a session is
  resumed, the store needs to rebuild from the log. Phase B
  needs an explicit "rebuild from session" path.

## Exit criteria (whole plan)

- [ ] All six phases land as separate commits.
- [ ] Default config preserves current behavior (active
      ceiling effectively unlimited until operator opts in).
- [ ] Opt-in mode (`active_ceiling_tokens` set) demonstrably
      keeps active context under ceiling on long sessions.
- [ ] Eval suite (Plan 07) measures the win on at least
      three long-context scenarios across two model sizes.
- [ ] PR 1's 14 REPL behavior tests pass at every phase.
- [ ] `cargo test --workspace` + clippy + fmt clean at every
      phase.

## Deferred (within Shape 1)

- Per-tool external scope (e.g., "all `web_read` results from
  this run"). Phase B's by_tool index supports this; we just
  haven't named a `RecurseScope` variant for it. Add when a
  consumer needs it.
- Cache of recurse results. Same scope + query asked twice
  should hit a cache. Useful but adds correctness surface;
  defer.
- Cross-session external context (the model recursing into
  a previous session). The session log persistence makes
  this trivially possible; needs a UX story (which session?
  privacy?) before shipping.
