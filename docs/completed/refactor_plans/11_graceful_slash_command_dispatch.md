# Plan 11 — Graceful slash-command dispatch

Short, urgent plan. Fixes a real TUI lockup and builds the contract
the autocomplete popup (plan 12) needs.

## Motivation

### The bug

Reproduction: open the TUI, type `/thinking bogus`, press Enter.
The output pane shows `Requested thinking level: bogus`, and then
the TUI stops responding. Ctrl+C still quits, but every slash
command and prompt submission after this point is silently dropped.

### Root cause

Tracing the flow end-to-end:

1. `anie-tui::app::handle_slash_command` (`app.rs:588-600`) matches
   `/thinking`, fires `UiAction::SetThinking(level)` via
   `action_tx.try_send(...)`, and optimistically prints
   `Requested thinking level: {level}` to the output pane **before
   the controller has validated anything**.
2. `InteractiveController::handle_action` (`controller.rs:217-225`)
   matches `UiAction::SetThinking` and calls
   `self.state.set_thinking(&level).await?`.
3. `ControllerState::set_thinking` (`controller.rs:505-513`)
   invokes `parse_thinking_level(requested)`
   (`controller.rs:863-871`), which returns
   `Err("invalid thinking level 'bogus'")` for anything outside
   `off|low|medium|high`. The `?` converts that into an
   `anyhow::Error` and propagates it.
4. `handle_action` returns `Err` (line 222).
5. In `run()` (`controller.rs:92` and `controller.rs:160`):
   `Some(action) => self.handle_action(action).await?` propagates
   the error out of the loop.
6. `run()` returns `Err`. The controller task exits. The TUI's
   `action_tx` is still alive but its receiver is gone; every
   subsequent `try_send` succeeds at the channel level but is never
   processed.
7. `agent_event_tx` is dropped with the controller task; the TUI
   sees no new events and appears frozen.

A single user typo takes down the entire control plane.

### Why this matters beyond `/thinking`

Every controller-owned `handle_action` arm that returns `Result` is
a candidate for the same failure mode:

- `UiAction::SetModel` (calls `state.set_model(&requested).await?`)
- `UiAction::SetResolvedModel` (same pattern)
- `UiAction::ReloadConfig { provider, model }`
- `UiAction::SwitchSession(session_id)` (invalid session id)
- `UiAction::Compact` (auto-compaction errors)
- `UiAction::NewSession`

Any of these, given bad user input or a transient provider failure,
can kill the controller loop. The `?` propagation is the bug.

### Secondary problem: no arg-level validation contract

The `CommandRegistry` (`anie-cli/src/commands.rs`) already exists
and knows name + summary. It does **not** yet know:

- Which commands take arguments.
- What the accepted argument values are.
- How to describe the argument shape to users (argument hint
  string).
- How to validate an argument before dispatch.

That contract is the scaffolding plan 12 needs to render inline
argument hints and argument-value completions. Without it, plan
12 has to duplicate the `off|low|medium|high` enum (and equivalent
for other commands) in the TUI.

## Scope

This plan does two things and only these two things:

1. **Containment.** Make `handle_action` log-and-report user-input
   errors instead of propagating them. The controller task must
   outlive any single malformed command.
2. **Contract.** Extend `SlashCommandInfo` with an argument
   specification so that (a) the TUI can validate bogus arguments
   before they are dispatched, and (b) plan 12 has the metadata it
   needs to render hints and argument-value completions.

**Out of scope:**

- The autocomplete popup UI itself (plan 12).
- New slash commands (roadmap items #8 `/resume` `/session` `/name`,
  #11 `/settings`).
- Extension-registered commands (plan 10).
- Changing the set of builtin commands.

## Design principles

1. **Validate before dispatch.** Every user-visible error path that
   can be caught in the TUI with a one-line comparison should be
   caught there and reported as a system message. The controller
   should only see well-formed requests.
2. **Controller failures are reported, not fatal.** A failure in
   `handle_action` is exactly as disruptive as a failed tool call:
   log it, surface it to the user, keep running.
3. **Exit criteria distinguish "user error" from "controller
   error."** User errors (bad arg, unknown session id) are not
   reasons to crash. Unexpected internal errors (state corruption,
   deadlock) still should be.
4. **One source of truth for command metadata.** The
   `CommandRegistry` owns name + summary + argument spec; the TUI
   reads it; `/help` renders it. No parallel enum table.

---

## Phase A — Contain `handle_action` errors

**Goal:** A malformed user command never kills the controller.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Split `handle_action` into a wrapper that classifies errors, so user-input errors emit a system message and return `Ok(())`. Only unrecoverable errors propagate. |
| `crates/anie-cli/src/controller_tests.rs` | Regression test: `/thinking bogus` does not terminate the controller loop; subsequent actions still fire. |

### Sub-step A — New `UserCommandError`

Introduce a user-facing error wrapper used by the slash-command
arms:

```rust
/// An error caused by malformed user input, rather than by a bug
/// or external failure. These are surfaced as system messages and
/// never terminate the controller loop.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UserCommandError {
    #[error("invalid thinking level '{0}' (expected: off, low, medium, high)")]
    InvalidThinkingLevel(String),
    #[error("unknown session '{0}'")]
    UnknownSession(String),
    #[error("{0}")]
    Other(String),
}
```

`parse_thinking_level` already returns `Result<ThinkingLevel, String>`;
map its `Err` through `UserCommandError::InvalidThinkingLevel` at
the call site in `set_thinking`.

### Sub-step B — `handle_action` wrapper

Wrap every arm that currently uses `?` on user-input-derived
operations. Two patterns exist:

```rust
async fn handle_action(&mut self, action: UiAction) -> Result<()> {
    match self.try_handle_action(action).await {
        Ok(()) => Ok(()),
        Err(HandleError::User(user_err)) => {
            self.send_system_message(&user_err.to_string()).await;
            Ok(())
        }
        Err(HandleError::Fatal(e)) => Err(e),
    }
}
```

Where `HandleError` is:

```rust
enum HandleError {
    User(UserCommandError),
    Fatal(anyhow::Error),
}
```

Every arm that today does `self.state.set_thinking(&level).await?`
becomes:

```rust
self.state
    .set_thinking(&level)
    .await
    .map_err(HandleError::from_user)?;
```

with `.from_user` classifying errors derived from user arguments.
Arms where the only possible error is internal (e.g., `Quit`,
`ClearOutput`) need no change.

### Sub-step C — The `/thinking` arm also updates the status bar

Today, `app.rs:597-599` optimistically prints
`Requested thinking level: {level}` before validation. After this
phase, the controller emits a **success** system message (e.g.
`Thinking level set to medium`) on success and a **rejection**
system message on failure. Remove the optimistic print in
`app.rs`.

### Test plan

| # | Test (in `controller_tests.rs`) |
|---|---|
| 1 | `invalid_thinking_level_emits_system_message` — enqueue `UiAction::SetThinking("bogus".into())`, run one iteration of the loop, assert a `SystemMessage` event with the expected wording is emitted. |
| 2 | `invalid_thinking_level_does_not_terminate_controller` — after the bad action, enqueue `UiAction::GetState`, assert a second `SystemMessage` (the state dump) arrives. |
| 3 | `valid_thinking_level_updates_state` — `SetThinking("high".into())` updates `state.config.current_thinking()` and emits a `StatusUpdate`. |
| 4 | `unknown_session_switch_is_reported_not_fatal` — `SwitchSession("nope".into())` emits a system message and leaves the controller live. |
| 5 | `internal_error_still_propagates` — inject an internal failure (e.g. a poisoned mutex if such a seam exists, otherwise a mock) and assert the controller exits. Skip this test if no seam exists; document it as a follow-up. |

### Exit criteria

- [ ] Every `handle_action` arm that takes user-supplied strings
      classifies errors into `User` vs `Fatal`.
- [ ] `controller_tests.rs` includes tests 1–4 above, all passing.
- [ ] Manual verification: `anie` in interactive mode, `/thinking foo`
      shows a helpful error and the next command still works.

---

## Phase B — Argument spec on `SlashCommandInfo`

**Goal:** The registry knows what arguments each command accepts,
so the TUI can validate (phase C) and plan 12 can render hints
and argument completions.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/commands.rs` | Add `argument_hint: Option<&'static str>` and `arguments: ArgumentSpec` to `SlashCommandInfo`. Update `builtin_commands()`. |
| `crates/anie-cli/src/commands.rs` | Extend `format_help()` to render the argument hint inline. |
| `crates/anie-cli/src/controller_tests.rs` | Extend `registry_covers_every_dispatched_slash_command` to assert argument specs are populated for commands that take arguments. |

### Sub-step A — `ArgumentSpec` type

```rust
/// Description of what arguments a slash command accepts.
#[derive(Debug, Clone)]
pub(crate) enum ArgumentSpec {
    /// The command takes no arguments. Any trailing text is an
    /// error.
    None,
    /// The command accepts a single free-form argument (model ID,
    /// session name, etc.). Validation happens at dispatch time.
    FreeForm {
        /// Whether an argument is required. If false, the command
        /// is still valid with no argument.
        required: bool,
    },
    /// The command accepts one of a fixed set of values.
    Enumerated {
        values: &'static [&'static str],
        /// Whether an argument is required. If false, running the
        /// command with no arg is valid (e.g. `/thinking` prints
        /// the current value).
        required: bool,
    },
    /// The command has subcommands (e.g. `/session`, `/session list`,
    /// `/session <id>`). Validation is delegated to the dispatcher.
    Subcommands {
        known: &'static [&'static str],
    },
}
```

### Sub-step B — Populate the builtin catalog

Update `builtin_commands()` with argument specs based on the
current behavior of `handle_slash_command` in `anie-tui::app`:

| Command | `arguments` | `argument_hint` |
|---|---|---|
| `model` | `FreeForm { required: false }` | `[<provider:id>\|<id>]` |
| `thinking` | `Enumerated { values: &["off","low","medium","high"], required: false }` | `[off\|low\|medium\|high]` |
| `compact` | `None` | `None` |
| `fork` | `None` | `None` |
| `diff` | `None` | `None` |
| `new` | `None` | `None` |
| `session` | `Subcommands { known: &["list"] }` | `[list\|<id>]` |
| `tools` | `None` | `None` |
| `onboard` | `None` | `None` |
| `providers` | `None` | `None` |
| `clear` | `None` | `None` |
| `reload` | `None` | `None` |
| `copy` | `None` | `None` |
| `help` | `None` | `None` |
| `quit` | `None` | `None` |

### Sub-step C — `format_help` rendering

`format_help()` currently prints `/{:<12} {summary}`. Change to:

```text
/{:<12} {argument_hint:<24} {summary}
```

omitting the column when `argument_hint` is `None`.

Example:
```text
/thinking     [off|low|medium|high]    Set reasoning effort
```

### Sub-step D — Validation helper

Add a pure helper on `SlashCommandInfo`:

```rust
impl SlashCommandInfo {
    /// Validate a raw argument string against this command's spec.
    /// Returns an error suitable for display as a system message.
    pub(crate) fn validate(&self, arg: Option<&str>) -> Result<(), String> {
        match &self.arguments {
            ArgumentSpec::None => {
                if arg.is_some() {
                    return Err(format!(
                        "/{} takes no arguments",
                        self.name
                    ));
                }
                Ok(())
            }
            ArgumentSpec::Enumerated { values, required } => {
                match arg {
                    None if *required => Err(format!(
                        "/{} requires one of: {}",
                        self.name,
                        values.join(", ")
                    )),
                    None => Ok(()),
                    Some(v) if values.iter().any(|c| c.eq_ignore_ascii_case(v)) => Ok(()),
                    Some(v) => Err(format!(
                        "/{} does not accept '{v}' (expected: {})",
                        self.name,
                        values.join(", ")
                    )),
                }
            }
            ArgumentSpec::FreeForm { required } => {
                match arg {
                    None if *required => Err(format!(
                        "/{} requires an argument",
                        self.name
                    )),
                    _ => Ok(()),
                }
            }
            ArgumentSpec::Subcommands { .. } => Ok(()),
        }
    }
}
```

### Test plan

| # | Test |
|---|---|
| 1 | `argument_spec_enumerated_rejects_unknown` — `SlashCommandInfo::validate` rejects `/thinking bogus` with a message listing the accepted values. |
| 2 | `argument_spec_enumerated_accepts_case_insensitive` — `HIGH`, `High`, `high` all pass. |
| 3 | `argument_spec_none_rejects_trailing_arg` — `/compact something` rejects. |
| 4 | `argument_spec_freeform_optional_allows_missing` — `/model` with no arg is valid. |
| 5 | `format_help_renders_argument_hint_column` — `/help` output includes `[off\|low\|medium\|high]` alongside `thinking`. |
| 6 | `builtin_catalog_includes_argument_spec_for_every_command` — table test that every builtin in `dispatched` has the expected variant. |

### Exit criteria

- [ ] `SlashCommandInfo` has `argument_hint` and `arguments` fields,
      populated for every builtin.
- [ ] `format_help` renders the hint column.
- [ ] `SlashCommandInfo::validate` is implemented and exercised by
      unit tests.

---

## Phase C — TUI-side pre-dispatch validation

**Goal:** The TUI catches arg errors locally and never sends a
known-bad `UiAction` to the controller. Failures in the controller
remain rare and are treated as bugs, not typos.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/lib.rs` | Expose a minimal `CommandCatalog` trait (or simply accept `Vec<CommandMetadata>`) that the App can consult during `handle_slash_command`. |
| `crates/anie-tui/src/app.rs` | `handle_slash_command` looks up the command in the injected catalog; on unknown command or failed `validate`, print a system message and return. |
| `crates/anie-cli/src/interactive_mode.rs` | Pass the registry-derived metadata into `App::new`. |
| `crates/anie-cli/src/commands.rs` or a new small crate | Define the metadata struct if it cannot live in `anie-cli` (the TUI cannot depend on the CLI crate). |
| `crates/anie-tui/src/tests.rs` | Coverage tests for unknown command + invalid argument. |

### Sub-step A — Resolve the crate-dependency direction

`anie-cli` depends on `anie-tui`; not the other way around. The
`CommandRegistry` lives in `anie-cli`, but the TUI needs read
access to the metadata. Two options; pick one:

- **Option 1 (preferred): move metadata types into `anie-tui`.**
  `SlashCommandSource`, `SlashCommandInfo`, `ArgumentSpec`,
  `SlashCommandInfo::validate`, and `builtin_commands()` move from
  `anie-cli/src/commands.rs` into a new
  `anie-tui/src/commands.rs`. `anie-cli` re-exports them. The
  `CommandRegistry` stays in `anie-cli` because it is
  dispatch-coupled; the metadata itself moves.
- **Option 2: new tiny `anie-commands` crate.** Used only for the
  metadata types. More ceremony; only justified if plan 10 or plan
  12 end up needing a shared home for extension-supplied metadata.

Pick option 1 unless plan 10 has concrete evidence it needs option 2.
Document the decision in the PR description; update `ROADMAP.md`
and `remaining_work_notes.md` if it changes plan 10's preconditions.

### Sub-step B — App owns a `CommandCatalog`

```rust
pub struct App {
    // existing fields...
    commands: Vec<SlashCommandInfo>,
}

impl App {
    pub fn new(
        event_rx: mpsc::Receiver<AgentEvent>,
        action_tx: mpsc::Sender<UiAction>,
        initial_models: Vec<Model>,
        commands: Vec<SlashCommandInfo>,
    ) -> Self { ... }
}
```

The CLI passes `command_registry.all().to_vec()` at startup.

### Sub-step C — Validate in `handle_slash_command`

Replace the current flat `match cmd { ... }` with:

1. Parse the `/name [args...]` shape.
2. Look up `name` in `self.commands`. If unknown, print
   `Unknown command: {name}. Type /help for available commands.`
   and return (same as today).
3. Call `command_info.validate(arg)`. On error, print the error and
   return — **do not** send a `UiAction`.
4. On success, dispatch to the existing per-command logic.

The existing per-command match then only handles well-formed
requests. Remove the arm-level argument checks that are now
redundant (e.g. the manual "no level provided" branch in
`/thinking`).

### Sub-step D — Keep optimistic logging in sync with validation

Before this plan, `/thinking medium` prints
`Requested thinking level: medium` optimistically. After phase A,
the controller emits the success/failure message. Remove the
optimistic print so we don't double-log on success or report
success prematurely on malformed input.

### Test plan

| # | Test (in `anie-tui/src/tests.rs`) |
|---|---|
| 1 | `slash_thinking_invalid_does_not_emit_uiaction` — inject a catalog containing the builtin `thinking` spec, submit `/thinking bogus`, assert the output pane shows the expected error and `action_rx.try_recv()` is empty. |
| 2 | `slash_thinking_valid_emits_uiaction` — submit `/thinking high`, assert `UiAction::SetThinking("high")` is sent. |
| 3 | `slash_compact_with_arg_is_rejected_locally` — `/compact foo` emits an error system message; no action is dispatched. |
| 4 | `slash_unknown_command_is_reported` — same message as today. |

### Exit criteria

- [ ] `App::new` accepts a catalog; `interactive_mode` wires it.
- [ ] `handle_slash_command` validates before dispatch.
- [ ] Malformed commands never hit `handle_action`.
- [ ] All existing `anie-tui/src/tests.rs` and
      `controller_tests.rs` tests still pass.

---

## Phase ordering

A → B → C. Phase A stands alone; it fixes the lockup immediately
and is worth shipping even if B and C slip. Phase B is pure
metadata and has no behavior change. Phase C depends on B.

## Out of scope

- The autocomplete popup (plan 12).
- Moving builtin command dispatch into the registry (that's an
  extension-system concern; plan 10).
- Persisting a per-command history of failed invocations.

## Preconditions for plan 12

Plan 12 (inline autocomplete popup) assumes:

- `App` can read `SlashCommandInfo` including `argument_hint` and
  `arguments`.
- `SlashCommandInfo::validate` exists for argument-level rejection.
- The metadata types live in `anie-tui` or a shared crate
  importable by the TUI.

All three are delivered by this plan's phase C.
