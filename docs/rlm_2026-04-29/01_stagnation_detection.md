# Plan 01 — Stagnation detection + aggressive compaction

**Branch:** `main` (this is the immediate next change).
**Status:** ready to ship.

## Rationale

The current mid-turn `ControllerCompactionGate` (PR 8.4 of
`docs/midturn_compaction_2026-04-27/`) only protects against
running out of compaction budget. The recently-raised cap
(500) makes that protection effectively unlimited. The
*real* failure mode that should stop compaction has nothing
to do with a count: it's when each successive compaction
fails to make meaningful progress.

Two stagnation patterns matter:

- **Converging floor.** The summarizer keeps doing its job —
  `tokens_after < tokens_before` on every call — but the
  "kept recent" tail (default 20k tokens) is itself above
  the threshold, so each iteration shrinks by less than 10%
  and the result is still over the line. The model
  legitimately needs context that exceeds what fits.
- **Regression.** `tokens_after >= tokens_before` (or
  monotonically growing across calls). The summarizer is
  broken or adversarial — its "summary" is bigger than the
  input.

The right action differs by kind:

- **Converging floor → tighten the knobs.** Halve
  `keep_recent_tokens` for the next compaction. The
  summarizer keeps the same prompt; the cut-point heuristic
  eats more of the recent window. `tokens_after` drops
  meaningfully. Repeat (with floor) on consecutive
  stagnation events.
- **Regression → fail fast.** Skip with a clear reason
  ("summarizer regressing"); the reactive-overflow path
  takes over. Aggressive compaction won't help here — the
  summarizer is the bug.

This plan implements both detection kinds and the per-kind
response. The 500-cap from the prior commit becomes the
nuclear-option backstop; the stagnation detector is the
real safeguard.

## Design

### State

`ControllerCompactionGate` gains one new field:

```rust
pub state: Arc<Mutex<GateState>>,

#[derive(Default)]
struct GateState {
    /// Last N compaction outcomes for stagnation detection.
    /// Bounded; older entries roll off.
    history: VecDeque<CompactionOutcome>,
    /// 0 = use config default; each level halves
    /// keep_recent_tokens for the next compaction. Capped at
    /// `MAX_AGGRESSIVE_LEVEL`. Decremented on a meaningful-
    /// progress compaction so we recover when the model's
    /// usage settles.
    aggressive_level: u8,
}

#[derive(Debug, Clone, Copy)]
struct CompactionOutcome {
    tokens_before: u64,
    tokens_after: u64,
}
```

The `Mutex` is fine: `maybe_compact` already does an async
LLM call that dwarfs lock contention.

### Detection

Pure function over the history slice. Looks at the last 3
outcomes (need at least 3 to call it stagnation):

```rust
const STAGNATION_WINDOW: usize = 3;
const STAGNATION_PROGRESS_THRESHOLD: f64 = 0.10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagnationKind {
    /// Each of the last N compactions shrunk by less than
    /// STAGNATION_PROGRESS_THRESHOLD of its tokens_before.
    /// The summarizer is making real progress but the floor
    /// (kept_recent + summary frame) is itself above
    /// threshold. Action: aggressive compaction.
    ConvergingFloor,
    /// `tokens_after` grew across the last N calls. The
    /// summarizer is producing more than it consumes.
    /// Action: skip and let reactive-overflow take over.
    Regressing,
}

fn detect_stagnation(history: &VecDeque<CompactionOutcome>) -> Option<StagnationKind> {
    if history.len() < STAGNATION_WINDOW { return None; }
    let recent: Vec<_> = history.iter()
        .rev()
        .take(STAGNATION_WINDOW)
        .copied()
        .collect();

    // Regression takes precedence over convergence — both
    // can be true if the summarizer is also weak, but the
    // regression action (skip) is the safer call.
    let monotone_grow = recent.windows(2).all(|w| w[0].tokens_after >= w[1].tokens_after);
    if monotone_grow {
        return Some(StagnationKind::Regressing);
    }

    let weak_progress = recent.iter().all(|c| {
        let shrunk = c.tokens_before.saturating_sub(c.tokens_after);
        let ratio = shrunk as f64 / c.tokens_before.max(1) as f64;
        ratio < STAGNATION_PROGRESS_THRESHOLD
    });
    if weak_progress {
        return Some(StagnationKind::ConvergingFloor);
    }

    None
}
```

> Note: `recent` is collected in reverse-chronological order
> and the `windows(2)` test reads "newer.tokens_after >=
> older.tokens_after" — i.e., the newer entry didn't shrink.

### Response

Inside `maybe_compact`, *before* the budget check:

```rust
let stagnation = {
    let state = self.state.lock().expect("gate state");
    detect_stagnation(&state.history)
};
match stagnation {
    Some(StagnationKind::Regressing) => {
        return Ok(CompactionGateOutcome::Skipped {
            reason: format!(
                "compaction stagnated (summarizer regressing); falling through to reactive overflow path"
            ),
        });
    }
    Some(StagnationKind::ConvergingFloor) => {
        let mut state = self.state.lock().expect("gate state");
        state.aggressive_level = state
            .aggressive_level
            .saturating_add(1)
            .min(MAX_AGGRESSIVE_LEVEL);
    }
    None => {}
}
```

When building the per-call config, halve `keep_recent_tokens`
according to the level (with floor):

```rust
const MAX_AGGRESSIVE_LEVEL: u8 = 3;
const KEEP_RECENT_FLOOR: u64 = 2_048;

fn aggressive_keep_recent(default: u64, level: u8) -> u64 {
    let scaled = default >> level as u32;
    scaled.max(KEEP_RECENT_FLOOR)
}
```

After a successful compaction, append to history and
recover the level on meaningful progress:

```rust
let outcome = CompactionOutcome { tokens_before, tokens_after };
let mut state = self.state.lock().expect("gate state");
state.history.push_back(outcome);
while state.history.len() > MAX_HISTORY {
    state.history.pop_front();
}
let shrunk = tokens_before.saturating_sub(tokens_after);
let ratio = shrunk as f64 / tokens_before.max(1) as f64;
if ratio >= 0.30 {
    state.aggressive_level = state.aggressive_level.saturating_sub(1);
}
```

`MAX_HISTORY = 8` is enough to never grow unbounded; we only
ever look at the last 3.

### `keep_recent_tokens` is the load-bearing knob

The reason halving `keep_recent_tokens` is the right
aggressive action and not, say, switching summarizer prompts:

- The cut-point heuristic (`find_cut_point` in
  `anie-session/src/lib.rs`) decides what's "recent enough
  to keep verbatim" by counting tokens backwards from the
  end. Smaller `keep_recent_tokens` = earlier cut = more of
  the conversation gets summarized.
- The summarizer's output size is roughly fixed (it produces
  a summary, not a verbatim copy), so shrinking the
  kept-recent tail directly shrinks `tokens_after`.
- It's a single numeric knob with no prompt-engineering
  variability — predictable behavior, easy to test.

A "very-terse-summarizer" prompt variant is a possible
future enhancement, but the data we have (real qwen3.5:9b
sessions) suggests `keep_recent_tokens` already does most of
the work.

## Files to touch

- `crates/anie-cli/src/compaction_gate.rs` — add state field,
  detection function, response logic, history-append after
  compaction, level recovery.

That's it. No new public types; no protocol change; no
config schema bump.

## Test plan

In `crates/anie-cli/src/compaction_gate.rs::tests`, add:

| # | Test | Asserts |
|---|------|---------|
| 1 | `stagnation_not_detected_with_fewer_than_3_outcomes` | `detect_stagnation` returns `None` for histories of length 0, 1, 2 even if those entries look stagnant. |
| 2 | `converging_floor_detected_when_progress_is_under_10pct` | Three outcomes each shrinking ~5% return `ConvergingFloor`. |
| 3 | `regressing_detected_when_tokens_after_grows` | Three outcomes with monotonically increasing `tokens_after` return `Regressing`. |
| 4 | `regressing_takes_precedence_over_converging` | A history that satisfies both predicates returns `Regressing`. |
| 5 | `gate_aggressive_compaction_halves_keep_recent` | Build a gate, push 3 weak-progress outcomes, call `maybe_compact`, assert the summarizer was called with `keep_recent_tokens` = config.keep_recent_tokens / 2. |
| 6 | `gate_aggressive_level_climbs_on_repeated_stagnation` | Drive 6 weak-progress compactions; assert `keep_recent_tokens` halves each round, with a floor at `KEEP_RECENT_FLOOR`. |
| 7 | `gate_aggressive_level_recovers_on_meaningful_progress` | Drive 3 weak-progress compactions (level 1), then a 50%-shrink compaction (recovers to level 0), then verify next compaction uses default `keep_recent_tokens`. |
| 8 | `gate_skips_with_regression_reason_when_summarizer_regressing` | Drive 3 regressing outcomes, call `maybe_compact`, assert `Skipped { reason }` contains "regressing". |
| 9 | `gate_history_bounded_at_max_history` | Drive 20 compactions; assert history length stays at `MAX_HISTORY`. |

For #5–#7 the existing `StubSummarizer` doesn't expose
`keep_recent_tokens`. Either:

- Inspect the message-slice that gets summarized (its size
  bounds the cut point implicitly), or
- Capture the `CompactionConfig` passed into a new
  `RecordingSummarizer` test stub.

Prefer the second — explicit and clear under inspection.

## Risks

- **Stagnation false positives.** A model with bursty
  context (long quiet turns followed by big tool dumps)
  might trip 3 weak-progress in a row early in a run. The
  aggressive response is bounded (3 levels) and recovers on
  meaningful progress, so worst case the next 1–2
  compactions cut tighter than needed. Acceptable.
- **`keep_recent_tokens = floor` is still too big.** If
  even the floor (2048) plus the summary frame is over
  threshold, aggressive compaction can't help. The
  `ConvergingFloor` detection at level 3 (floor) should
  promote to `Regressing` semantics — i.e., skip. The
  current design doesn't handle this; in practice the
  reactive-overflow path will catch it. Add as a follow-up
  if needed.
- **Mutex contention.** `Arc<Mutex<GateState>>` is held
  across `detect_stagnation` (pure, fast) and across the
  outcome append (also fast). The async LLM call happens
  outside the lock. Should be uncontended.

## Exit criteria

- [ ] `ControllerCompactionGate` carries `state: Arc<Mutex<GateState>>`.
- [ ] `detect_stagnation` is a pure function with the 9 tests above passing.
- [ ] On `ConvergingFloor` detection, the next compaction call uses halved `keep_recent_tokens`.
- [ ] On `Regressing` detection, the gate returns `Skipped` with a reason that contains "regressing".
- [ ] After a meaningful (>=30%) compaction, `aggressive_level` decreases.
- [ ] PR 1's 14 REPL behavior tests pass unchanged.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo fmt --all -- --check` clean.

## Deferred

- Aggressive summarizer-prompt swap (in addition to
  `keep_recent_tokens` halving). Single knob is enough for
  now; revisit if eval data shows the prompt is the
  bottleneck.
- `Skipped` reason variations beyond "regressing" /
  "budget exhausted." A more specific reason for "floor hit"
  could be useful but isn't blocking.
- User-visible `SystemMessage` when stagnation fires. The
  controller already emits a system message when
  `Skipped` fires; that covers it.
