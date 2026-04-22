# Plan 04 — Streaming coalescing + backpressure

## Rationale

Streaming responses arrive at 60–200 tokens/sec. Each
`TextDelta` event appends to the current assistant block's
text, invalidates its cache, and marks `dirty`. The render
loop's 30 fps cap (`app.rs:1468–1480`) correctly coalesces
these into at most ~30 draws/sec — but each of those draws
still rewraps the entire streaming block from scratch because
its `text` field has grown by however many deltas arrived in
the interval.

After Plan 03 lands, the per-rewrap cost will drop by ~30%+,
but it's still O(text_len) per frame. For a long streaming
response (10k chars) that's non-trivial.

Two related improvements:

1. **Apply deltas in a single batch per frame.** Today, each
   delta individually appends + invalidates. Pending deltas
   accumulate in the channel between frames — we can drain
   them all at the top of the render tick, apply them as one
   append, and invalidate once.
2. **Bound the agent→UI channel.** If the provider is
   streaming faster than we can render (hundreds of deltas
   queued), apply backpressure rather than letting the queue
   grow unbounded. Start with a generous bound (256) and
   adjust from benchmarks.

Neither of these is strictly necessary if Plan 03 lands well
— but together they cut the worst-case cost of a fast stream
and put a predictable ceiling on channel memory. This plan is
a "polish" tier rather than a critical fix.

## Design

### 4.1 Drain-and-apply at the top of `draw`

Before `app.render(frame)`, drain all pending events from the
agent channel (the `event_rx` loop at `app.rs:1516–1524`)
into a small vec. Group consecutive `TextDelta` events per
block-id and apply them as one `String::push_str` + single
`invalidate_last` call.

Non-text events (tool-start, tool-end, compaction-start, etc.)
keep their existing per-event handling.

### 4.2 Bounded mpsc channel

The agent → UI event channel is currently unbounded. Switch
to `tokio::sync::mpsc::channel(256)`. The sender side
(agent-loop) already awaits; making it bounded just gates the
producer on a full channel.

The bound is a starting guess. Tune from Plan 01 benchmarks
if rendering is slower than production.

### 4.3 Keep the existing dirty/tick machinery

Do not change the frame-budget cap, dirty flag, or idle tick
(already correctly tuned per Agent C). This plan is strictly
about what happens when deltas fire.

## Files to touch

- `crates/anie-tui/src/app.rs`: the main `tokio::select!`
  loop and its event-drain code (~1516–1524 per the audit).
- Wherever the agent-side channel is created (likely
  `crates/anie-cli/src/controller.rs` or `anie-agent`).
  Verify the sender-side cost of `await` on a full bounded
  channel is acceptable.
- `crates/anie-tui/src/output.rs`: a new
  `append_batch_to_last_assistant(&str)` that collapses
  multiple deltas into one append + one invalidation.

## Phased PRs

### PR-A: drain-and-batch deltas per frame

- Implement the top-of-draw drain.
- Keep the channel unbounded for now.
- Exit: `stream_into_static_600` bench p95 drops; number of
  `invalidate_last` calls per second drops from ~token-rate
  to ~frame-rate.

### PR-B: bounded channel + backpressure

- Switch the agent→UI channel to `channel(256)`.
- Verify the sender path awaits correctly.
- Exit: under a synthetic fast-stream (1000 deltas in 1s),
  memory growth of the channel is bounded. No deadlock.

## Test plan

- **Behavior parity.** A scripted stream of 100 deltas
  produces identical final transcript text and identical
  streaming-block display at every frame boundary (same
  prefix at every render tick).
- **Batching assertion.** Instrument `invalidate_last` with
  a counter during a 100-delta test. Before PR-A: ≥100. After
  PR-A: between 1 and (100 / frame_rate × test_duration).
- **Backpressure test** for PR-B: a sender that writes 1000
  events into a `channel(256)` completes without deadlocking
  and the UI-side consumer sees all 1000 events in order.
  Sender's total wall time ≤ test budget.
- **Existing integration tests pass.** The agent-loop shape
  doesn't change; the only observable difference is when
  `dirty` gets set.

## Risks

- **Reordering hazard.** If we drain all events before
  applying and a non-text event arrives between two text
  deltas, naïvely collapsing all text deltas would reorder.
  The batch must respect original ordering — collapse only
  **consecutive** deltas for the same block-id.
- **Channel bound too small.** If 256 is too tight, the
  agent stalls visibly. Start at 256, raise to 1024 if
  benches show it.
- **Backpressure on tool-start events.** We don't want a
  full channel to stall tool orchestration. If we observe
  this, split the agent→UI stream into two channels: one
  for deltas (bounded), one for control events (unbounded
  or loose bound).
- **Sender-side async context.** If any sender site holds a
  non-Send lock across the `await`, the bounded switch
  surfaces it. Audit before merging.

## Exit criteria

- [ ] Both PRs landed.
- [ ] `invalidate_last` call count per second drops by
      ≥5× for the `stream_into_static_600` scenario.
- [ ] `stream_into_static_600` p95 frame time is ≤ what
      Plan 03 achieves.
- [ ] No reordering in the batched apply — regression test
      exercising interleaved text+tool events.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.

## Deferred

- **Adaptive channel bound** that shrinks when rendering
  is keeping up and grows under burst. Over-engineering until
  we see real starvation.
- **Multi-channel split** for delta vs control events. Only
  do this if backpressure on control events surfaces as a
  symptom.
- **60 fps target.** Stated in README — unnecessary. Don't
  change the frame budget here.
