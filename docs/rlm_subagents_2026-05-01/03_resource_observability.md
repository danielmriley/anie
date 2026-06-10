# PR 3 — Resource observability for sub-agents

## Rationale

Sub-agents now run real work (PR 2 — they inherit tools).
Real work has cost: tokens spent against the model API,
wall-clock the user is waiting on, tool calls that
modified files or hit the network. The harness should
make these visible so:

- The user can see what each recurse call cost.
- A future budgeting / capping layer (deferred per
  series principle) has the data to act on.
- Smoke comparisons can quantify "decompose helped" vs
  "decompose just spent more tokens for the same
  outcome."

Per the series principle: **observability, not caps.**
This PR surfaces stats; it doesn't bound anything.

## Design

When the recurse tool finishes its sub-agent loop,
read off:

- **Wall-clock:** `Instant::now()` before
  `start_run_machine`, elapsed at `finish`.
- **Tokens:** sum `input_tokens`, `output_tokens`,
  `cache_read_tokens`, `cache_write_tokens` across
  every `Message::Assistant` in
  `sub_result.generated_messages`. The provider layer
  populates `Usage` per message; we just aggregate.
- **Tool calls:** count `Message::ToolResult` entries
  in the same vec — one per completed sub-agent tool
  call.
- **Cost:** sum `usage.cost.total` across assistants.

Surface in the recurse tool's `result.details` JSON
under stable keys (`sub_agent_elapsed_ms`,
`sub_agent_input_tokens`, …). The keys land alongside
existing fields like `depth` and `scope_kind`. A
future TUI / status-bar PR can pluck these keys to
render a live "recurse cost: 12k tokens, 3.1s" segment.

Also info!-level log per recurse done (extends the
existing `recurse done` line with the new fields).

## Why surface in the tool result

Two alternatives considered:

1. **Bubble through events.** A new
   `AgentEvent::SubAgentStats` would let consumers
   subscribe. More structured but adds an event
   variant + downstream plumbing — complexity for
   little gain at this stage.
2. **Aggregate in a controller-side accumulator.** A
   `RecurseResourceTracker` shared via `Arc` that the
   recurse tool writes into. Cleaner for "show me
   total recurse spend this session" UIs but requires
   plumbing the tracker through.

The chosen approach (details-in-result) is the
smallest viable surface that makes the data
discoverable. Future PRs can build either alternative
on top by reading the details and accumulating.

## Files to touch

- `crates/anie-tools/src/recurse.rs` — wrap the
  sub-agent run in a timer; aggregate Usage from
  `sub_result.generated_messages`; surface in
  `details`.
- New `SubAgentStats` private struct + tests.

Estimated diff: ~150 LOC of code, ~120 LOC of tests.

## Phased PRs

Single PR.

## Test plan

- `sub_agent_stats_sums_assistant_usage_across_messages`
  — multiple AssistantMessage entries, summed correctly.
- `sub_agent_stats_counts_tool_results_as_tool_calls`
  — `Message::ToolResult` entries count as tool calls.
- `sub_agent_stats_empty_messages_zero_tokens`
  — empty input → zero stats (no division by zero, no
  panic).
- `recurse_result_details_include_sub_agent_stats`
  — integration: the recurse tool's `details` payload
  has all the stats keys.

## Risks

- **Token reporting depends on provider.** Some
  providers don't populate `Usage` reliably. Stats
  show 0 in those cases — accurate but not useful.
  Mitigation: this is a provider-level concern;
  upstream fix at the provider, not here.
- **Cost reporting is best-effort.** Depends on the
  provider knowing the model's pricing. Mitigation:
  document that `cost_total: 0.0` means "unknown",
  not "free."
- **Wall-clock includes provider latency, not just
  model time.** A slow Ollama → high `elapsed_ms`
  doesn't mean the recurse was inefficient.
  Mitigation: surface alongside token stats so users
  can disambiguate.

## Exit criteria

- [ ] `SubAgentStats::from_messages` aggregates input
      and output tokens, tool calls, and cost across
      `generated_messages`.
- [ ] Recurse tool wraps `start_run_machine` →
      `finish` in a timer.
- [ ] `result.details` includes the six new stats
      keys.
- [ ] `recurse done` info-level log includes
      `elapsed_ms` + `input_tokens` + `output_tokens`
      + `tool_calls`.
- [ ] All four tests pass.
- [ ] `cargo test --workspace` + clippy clean.

## Deferred

- Aggregating across multiple recurse calls in a
  session (a `SessionRecurseTotals` struct that the
  controller maintains by reading every recurse
  tool result's details). Lands when the UI side
  needs it.
- Status-bar segment showing live recurse spend.
  Same TUI plumbing concern as PR 1's depth segment;
  defer until smoke shows demand.
- Hard caps on token spend per recurse / per session.
  Per series principle, only add if observability
  proves insufficient.
