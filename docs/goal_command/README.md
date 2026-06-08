# `/goal` — autonomous goal loop ("Ralph loop")

A `/goal <description>` command that puts the agent into an autonomous
loop: it works toward the goal, **re-prompts itself to continue after
every turn** (no fixed interval), and **stops when it judges the goal
achieved** (or blocked, or it hits a turn/budget cap). Modeled on OpenAI
Codex's `/goal` ("keep a goal alive across turns, don't stop until
achieved" — the "Ralph loop"). Sibling to `/loop` (docs/loop_command/),
which re-sends a *fixed* message on a *fixed interval*; `/goal`
continues immediately and self-terminates.

## 1. Rationale

`/loop` is mechanical (timer re-sends a message). `/goal` is the
genuinely-autonomous version rivals ship: give it a mission and a way to
verify success, and it iterates plan→act→observe→re-plan until done. The
research is unanimous that the key is a **verifiable terminal condition**
("a passing test suite, a generated file") and that the agent must be
able to **declare completion** — otherwise it runs in circles. anie has
no such mode today.

## 2. Design

### Surface
- `/goal <description>` — start (replaces any active goal/loop).
- `/goal stop` (`off`/`cancel`) — cancel.
- `/goal` (no arg) — status (goal text + turns remaining).

`/goal` and `/loop` are mutually exclusive — starting one clears the
other (two autonomous run-drivers would fight).

### Completion signal (the crux)
The goal framing instructs the model to end its message with an exact
sentinel when it is **done** or **blocked**:

- `GOAL_COMPLETE` — the goal is fully achieved and verified.
- `GOAL_BLOCKED: <reason>` — genuinely stuck; needs human input.

A pure `detect_goal_outcome(&[Message])` scans the completed run's
assistant text for these markers (Complete wins over Blocked). This is
model-agnostic and needs no new tool. A structured `goal_complete` tool
is a cleaner future refinement (§7).

### Continuation (controller)
New `goal_state: Option<GoalState> { goal, turns_remaining }`. The loop:
1. `/goal <g>` → set state (`turns_remaining = GOAL_MAX_TURNS`), clear any
   loop, and `start_prompt_run(initial_goal_prompt(g))`.
2. On each **clean** run completion (in the run-completion branch, where
   `result` is available), compute a deferred decision from
   `detect_goal_outcome(&result.generated_messages)` and stash it:
   - Complete / Blocked → `Stop(<message>)`.
   - otherwise → `Continue`.
3. After the existing queued-prompt drain (so user input always wins):
   - `Stop` → drop the goal, emit the outcome message (applies even if a
     follow-up queued — a finished goal is finished).
   - `Continue` → only if **no** follow-up started: if the turn cap is
     hit → stop (cap message); if `session_budget_block()` says the
     session ceiling is reached → stop (budget message); else decrement
     and `start_prompt_run(goal_continuation_prompt(g))`. If a follow-up
     *did* start, the goal naturally resumes after it completes.

Auto-continuation is immediate (no timer) — `/goal` reuses the run-
completion boundary the queued-prompt drain already uses, so it composes
with retries (a retrying run isn't "clean", so the goal waits for a real
completion) and with `/loop`'s timer (mutually exclusive anyway).

### Guardrails (anie-specific)
- **Turn cap** `GOAL_MAX_TURNS` (default 50 — each turn is a full,
  possibly-expensive agent run; lower than `/loop`'s 1000).
- **Budget** — a goal turn is an ordinary run, so `[budget]` ceilings
  apply; the loop also pre-checks `session_budget_block()` and stops
  cleanly when the session ceiling is reached.
- `/goal stop` always available; `Ctrl+C` aborts the active run as usual.

### Modes
Interactive-only. Only the TUI dispatches `UiAction::Goal`; the
controller's `try_handle_action` is the only exhaustive match to update.

## 3. Files to touch
| File | Change |
|------|--------|
| `crates/anie-tui/src/app.rs` | `UiAction::Goal(Option<String>)`; `/goal` dispatch |
| `crates/anie-cli/src/commands.rs` | register `/goal` + coverage list |
| `crates/anie-cli/src/controller.rs` | `GoalState`, `goal_state`, `pending_goal_decision`, `parse_goal_command`, `detect_goal_outcome`, prompts, `handle_goal_command`, goal advance in the run-completion branch; clear loop on goal start (and vice versa) |
| `crates/anie-cli/src/controller_tests.rs` | parse, detect, start/stop/status, decision tests |
| `docs/ROADMAP.md` | mark `/goal` shipped |

## 4. Test plan
- `parse_goal_command_*` (start / stop aliases / status / empty)
- `detect_goal_outcome_finds_complete_and_blocked_markers`
- `detect_goal_outcome_prefers_complete_over_blocked`
- `goal_command_starts_and_clears_any_active_loop`
- `goal_stop_clears_the_state`
- `next_goal_step_caps_turns_and_respects_budget` (pure decision fn)

## 5. Risks
- **Runs forever / in circles.** Mitigated by the turn cap, `[budget]`
  ceilings, the `GOAL_BLOCKED` bail, and `/goal stop`.
- **Model never emits the marker.** Falls back to the turn cap. The
  continuation prompt reinforces the marker convention every turn.
- **False-positive marker** (model mentions `GOAL_COMPLETE` in prose).
  Low risk; the convention is "end your message with the exact line." A
  future structured tool removes the ambiguity.

## 6. Exit criteria
- [ ] `/goal <desc>` runs autonomously, continuing after each turn.
- [ ] Stops on `GOAL_COMPLETE` / `GOAL_BLOCKED`, the turn cap, or budget.
- [ ] `/goal stop` cancels; `/goal` reports status; starting clears `/loop`.
- [ ] `cargo test --workspace` + `clippy -D warnings` + `fmt` green.

## 7. Deferred
- **Structured `goal_complete` tool** instead of a text sentinel.
- **Progress summarization / self-check cadence** (the agent periodically
  re-states progress).
- **Persisting an active goal across restart.**
- **`/goal` + `/loop` coexistence** (kept mutually exclusive for now).
