# 04 — Provider SSE streaming consolidation

**HIGH RISK** — defer until a clear pull (e.g., adding a 4th
provider with the same shape).

## The duplication

Three providers re-implement nearly-identical event-reassembly
state machines:

- `anie-providers-builtin/src/openai/streaming.rs` — 816 LOC
- `anie-providers-builtin/src/anthropic.rs` — ~600 LOC of
  stream-state code embedded in the file
- `anie-providers-builtin/src/ollama_chat/streaming.rs` —
  parallel implementation for NDJSON transport

Invariant logic across all three:
- Empty-assistant-response guards
- Tool-call buffering and indexing
- Usage accumulation on final events
- Thinking-block / reasoning-detail capture
- Stop-reason classification

Variant logic per provider:
- Wire format (SSE for OpenAI/Anthropic, NDJSON for Ollama)
- Event schema (delta keys, tool-call shape)
- Provider-specific retry hooks (Ollama has num_ctx halving,
  OpenAI has stream_options fallback)

## Proposed shape

A `SseStateMachine<S: SchemaAdapter>` trait where the
schema adapter handles wire-format parsing and event-type
mapping; the state machine handles invariant reassembly.

```rust
trait StreamSchema {
    type ChunkEvent;
    fn parse_chunk(line: &str) -> Result<Option<Self::ChunkEvent>>;
    fn classify(event: &Self::ChunkEvent) -> EventClass;  // delta, end, etc.
    fn extract_text(event: &Self::ChunkEvent) -> Option<&str>;
    // ... etc.
}

struct StreamReassembler<S: StreamSchema> {
    // Common state: tool-call buffer, usage, etc.
}
```

## Why high risk

Streaming is correctness-critical. A subtle change in
ordering or buffering can:
- Break replay-fidelity (tool-call IDs out of order)
- Corrupt usage counts (double-counting)
- Lose thinking-block signatures
- Change observable provider behavior

Each provider's tests cover its current path; a shared
abstraction adds combinatorial paths to verify.

## Approach when ready

1. Build `SseStateMachine` shape in `anie-provider/src/`
   alongside the existing trait.
2. Migrate ONE provider first (start with Ollama since it
   has the most-recent code touch).
3. Run all the integration tests.
4. Migrate OpenAI.
5. Migrate Anthropic.
6. Each migration is its own PR; never combine two.

Estimated savings: ~400 LOC after all three migrate.

## Trigger for revisiting

- A 4th provider with similar shape (e.g., a new
  OpenAI-compatible vendor) — that's when the duplication
  becomes painful enough to justify the shape work.
- A bug found in one provider's stream parser that should
  also affect the others — discover via
  cross-provider integration tests, then fix once via the
  shared abstraction.

Until then: leave well enough alone.
