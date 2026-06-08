# `/loop` — recurring-prompt command

A `/loop <interval> <message>` command that re-submits a prompt on a
fixed interval, so the agent keeps working a task across turns without
the user re-typing. Modeled on OpenAI Codex's proposed `/loop` (codex
issue [#15679](https://github.com/openai/codex/issues/15679)) and its
`/goal` "Ralph loop" philosophy ("keep a goal alive across turns, don't
stop until achieved").

## 1. Rationale

### The gap
anie has no way to keep an agent iterating on a goal unattended. The
user must re-submit "continue" after every turn. Rival harnesses ship
this: Codex's `/loop` (recurring prompt) and `/goal` (autonomous loop);
Claude Code's `/loop`. It is a high-leverage power-user feature for
long-running, self-correcting work (test-fix loops, "keep refactoring
until clean", overnight tasks).

### Evidence — Codex `/loop` (issue #15679)
- Syntax: `/loop <Xm> <message>` (e.g. `/loop 3m continue`,
  `/loop 5m summarize progress`).
- Strict interval parsing.
- **Does not interrupt a running turn** — "if a turn is already running,
  queue the next loop message instead of interrupting the active turn."
- Re-issuing `/loop` **replaces** the previous schedule (no stacking).
- **Self-removal**: "if a loop-triggered turn determines the work is
  already fully complete, the loop should remove itself."

anie already has the substrate to implement this with almost no new
machinery:
- a `tokio::select!` controller loop (`controller.rs:run`) whose
  `PendingRetry::Armed` arm already does a non-blocking
  `sleep_until(deadline)` timer — the exact precedent for a recurring
  loop timer;
- `UiAction::QueuePrompt(text)`, whose handler **already** implements
  "start now if idle, else queue onto the FIFO" — precisely Codex's
  non-interrupt semantics;
- a `queued_prompts: VecDeque<String>` FIFO drained at the run-completion
  boundary;
- the `/context-length` command as a one-arg dispatch precedent
  (`UiAction::ContextLength(Option<String>)`).

## 2. Design

### Command surface
- `/loop <interval> <message>` — start (or replace) the loop. Interval is
  `<N>m` (minutes) or `<N>s` (seconds); `N` a positive integer. The
  message is everything after the interval token.
- `/loop stop` (aliases `off`, `cancel`) — cancel the active loop.
- `/loop` (no argument) — show the active loop's interval + message +
  iterations remaining, or "No loop is active."

anie-specific deviation from Codex (documented): we also accept `s`
(seconds) intervals, because seconds are useful for short loops and make
the feature testable without minute-long waits. Codex is minutes-only.

### State + timer (controller)
A new `loop_state: Option<LoopState>` on `InteractiveController`:

```rust
struct LoopState {
    interval: Duration,
    message: String,
    next_fire: Instant,        // tokio::time::Instant
    fires_remaining: u32,      // safety cap; see below
}
```

The controller's main `select!` gains one arm in **all three** state
branches (run-active, retry-armed, idle), guarded by the loop being
armed:

```rust
_ = wait_until(self.next_loop_fire()) => { self.on_loop_fire().await?; }
```

where `wait_until(Option<Instant>)` is `sleep_until(t)` when `Some`, else
`future::pending()` (the arm is inert when no loop is armed). The idle
branch (today a bare `recv().await`) becomes a `select!` so the timer can
fire while idle.

`on_loop_fire`:
1. decrement `fires_remaining`, advance `next_fire = now + interval`;
2. dispatch the message via `UiAction::QueuePrompt(message)` — reusing
   the existing "start-if-idle / queue-if-busy" logic verbatim, which is
   the non-interrupt contract;
3. if `fires_remaining` hit 0, drop `loop_state` and emit a "loop
   stopped (iteration cap)" system message.

### Safety cap (anie-specific, not in Codex)
An autonomous recurring prompt is a foot-gun (runaway spend / infinite
loop). Every loop carries a hard iteration cap (`LOOP_MAX_ITERATIONS`,
default 1000 — high enough to be a pure runaway guard, not a normal
stopping mechanism). Normal termination is `/loop stop` (or self-removal,
deferred §8). The cap is surfaced in the start confirmation.

### Parsing (pure, testable)
A free `fn parse_loop_command(arg: Option<&str>) -> Result<LoopCommand,
String>` returning `Start { interval, message } | Stop | Status`, unit-
tested independently of the controller. Invalid interval / empty message
return a human-readable error shown as a system message.

### Status surfacing
v1 uses system messages at the lifecycle boundaries (started / fired /
stopped). A status-bar segment (`loop: 3m ×N`) is a clean follow-up but
adds `StatusUpdate` field plumbing; deferred to keep the first cut tight
(§8).

### Modes
`/loop` is interactive-only. Only the TUI dispatches `UiAction::Loop`;
`print`/`rpc` modes never construct it. The controller's
`try_handle_action` is the only exhaustive `UiAction` match to update.

## 3. Files to touch
| File | Change |
|------|--------|
| `crates/anie-tui/src/app.rs` | `UiAction::Loop(Option<String>)`; `/loop` dispatch arm |
| `crates/anie-cli/src/commands.rs` | register `/loop` in the builtin catalog + coverage test |
| `crates/anie-cli/src/controller.rs` | `LoopState`, `loop_state` field, `parse_loop_command`, `UiAction::Loop` handler, `on_loop_fire`, `next_loop_fire`, `wait_until`, timer arms in all 3 select branches |
| `crates/anie-cli/src/controller_tests.rs` | parse + handler + fire + cap tests |
| `docs/ROADMAP.md` | mark `/loop` shipped |

≤5 source files.

## 4. Phased PRs (one commit)
This is small enough for one focused commit, but logically:
1. `loop/1` — `parse_loop_command` + `LoopState` + the `UiAction::Loop`
   handler (start/stop/status) and `on_loop_fire`, with the timer arms
   wired. Tests for parse, start/stop/status, fire-queues-when-busy,
   fire-starts-when-idle (component-level), and the iteration cap.
2. `loop/2` — docs (ROADMAP).

## 5. Test plan
- `parse_loop_command_parses_minutes_and_seconds`
- `parse_loop_command_rejects_bad_interval_and_empty_message`
- `parse_loop_command_recognizes_stop_aliases_and_status`
- `loop_command_starts_and_replaces_schedule`
- `loop_stop_clears_the_schedule`
- `loop_fire_queues_message_when_a_run_is_active`
- `loop_fire_decrements_and_caps_iterations`

## 6. Risks
- **Runaway loop / spend.** Mitigated by the hard iteration cap + the
  existing `[budget]` ceilings (a loop run is an ordinary run, so
  `max_session_cost_usd`/`max_session_tokens` still apply and will halt
  it). `/loop stop` always available.
- **Loop fires during a retry backoff.** The fire goes through
  `QueuePrompt`, which already has defined semantics for the armed-retry
  state; acceptable for v1.
- **Timer starvation.** The arm is one `sleep_until` among the select; it
  competes fairly with UI actions and run completion, same as the retry
  timer.

## 7. Exit criteria
- [ ] `/loop 3m continue` re-submits "continue" every 3 minutes; queues
      (does not interrupt) when a turn is running.
- [ ] `/loop stop` cancels; re-issuing `/loop` replaces the schedule.
- [ ] `/loop` with no arg reports status.
- [ ] Iteration cap halts a runaway loop with a message.
- [ ] `cargo test --workspace` + `clippy -D warnings` + `fmt` green.
- [ ] `/loop` appears in `/help`.

## 8. Deferred
- **Self-removal on task completion** (Codex's "loop removes itself").
  Needs a reliable completion signal from the agent (a marker convention
  or a structured "done" tool); the heuristic version is unreliable.
  Ship the cap + `/loop stop` now; revisit with a completion convention.
- **Status-bar `loop:` segment** — needs `StatusUpdate` plumbing.
- **`/goal`-style fully-autonomous loop** (plan its own steps, no fixed
  interval) — a larger feature on top of the REPL-loop refactor.
- **Persisting an active loop across `/new` / restart** — a loop is
  in-memory, run-scoped; cleared on session switch.
