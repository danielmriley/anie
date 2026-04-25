# Plan 04 — terminal capability detection

**Tier 2 — small-medium, prerequisite for shipping markdown with
hyperlinks.**

## Rationale

pi probes the terminal before emitting escape sequences that not
every terminal understands (`packages/tui/src/terminal-image.ts:40`):

- Kitty image protocol support
- iTerm2 image protocol
- Sixel graphics
- WezTerm / Ghostty extended features
- OSC 8 hyperlinks
- tmux / screen passthrough (both swallow OSC by default)

anie emits plain ANSI without probing. That's safe today because
we render plain text, but the moment we add markdown with
hyperlinks (Plan 05), we'll be sending OSC 8 into tmux sessions
that eat them silently.

The fix is a capability probe + an API the rest of the TUI can
check before emitting anything advanced.

## Design

Mirror pi's `TerminalCapabilities` struct
(`packages/tui/src/terminal-image.ts`): three fields, each
already accounting for multiplexer / env conditions. Over-
engineering an eight-field struct proved tempting but pi shows
it isn't needed — the consumer wants yes/no answers, not a
breakdown of the signals that produced them.

```rust
pub struct TerminalCapabilities {
    /// Which inline-image protocol is supported, if any.
    /// `None` also covers "images advertised but we're inside
    /// tmux/screen without passthrough."
    pub images: Option<ImageProtocol>,
    /// 24-bit colour is advertised by the terminal.
    pub truecolor: bool,
    /// OSC 8 hyperlinks are safe to emit. False inside
    /// tmux/screen by default, true in most native terminals.
    pub hyperlinks: bool,
}

pub enum ImageProtocol {
    Kitty,
    Iterm2,
}
```

Detection is a pure function of the environment — no dynamic
DA/DA2 queries (those would interleave awkwardly with crossterm's
event stream for little payoff).

### Env vars we read

Matching pi's actual probe order:

- `TERM_PROGRAM` — primary signal; values include `iTerm.app`,
  `vscode`, `WezTerm`, `ghostty`, `Apple_Terminal`.
- `TERM` — string values starting with `tmux` or `screen` mark
  multiplexer use.
- `TMUX` — presence-check for tmux (more reliable than `TERM`).
- `KITTY_WINDOW_ID` — presence-check for Kitty.
- `GHOSTTY_RESOURCES_DIR` — presence-check for Ghostty.
- `ITERM_SESSION_ID` — presence-check for iTerm2.
- `WEZTERM_PANE` — presence-check for WezTerm.
- `COLORTERM` — `truecolor` or `24bit` values mean yes.

Multiplexer = `TMUX.is_some() || TERM.starts_with("tmux") ||
TERM.starts_with("screen")`. Hyperlinks are disabled under
multiplexers (even if the outer terminal supports them) because
passthrough isn't the default configuration.

### Override env vars (anie-specific, not in pi)

Documented extensions for users in unusual environments:

- `ANIE_FORCE_OSC8=1` — force hyperlinks on.
- `ANIE_FORCE_OSC8=0` — force hyperlinks off.

Omitted from MVP unless someone surfaces a concrete need. Pi
ships without equivalents and has not suffered for it.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tui/src/terminal.rs` | `TerminalCapabilities::detect()`, stored on `TerminalGuard`. |
| `crates/anie-tui/src/app.rs` | Wire `TerminalCapabilities` into `App` for widgets to read. |
| `crates/anie-tui/src/lib.rs` | Re-export. |

## PR

Single PR, small:

1. Add `TerminalCapabilities::detect()` in `terminal.rs`. Pure
   function of env vars; testable.
2. `TerminalGuard::new()` stores it; `App::new` receives it as
   a param.
3. Expose via `App::terminal_capabilities()` for widgets.
4. Tests cover the env-var matrix: set various `TERM_PROGRAM`
   values and assert the enum/flags.

## Test plan

| # | Test |
|---|------|
| 1 | `capabilities_detect_kitty_via_kitty_window_id` |
| 2 | `capabilities_detect_iterm_via_term_program` |
| 3 | `capabilities_detect_wezterm_via_executable` |
| 4 | `capabilities_detect_vscode_terminal` |
| 5 | `capabilities_detect_tmux_via_tmux_env` |
| 6 | `capabilities_detect_screen_via_term_screen` |
| 7 | `capabilities_osc8_disabled_inside_tmux_by_default` |
| 8 | `capabilities_truecolor_on_colorterm_truecolor` |
| 9 | `capabilities_unknown_emulator_is_conservative` — default to all-off when we can't identify |

Tests can seed `std::env::set_var` within a `#[serial]` block
(from the `serial_test` crate) or take an owned env-map input for
pure-function testing. Prefer the pure-function approach — no
global-state mutation in tests.

## Risks

- **Env-var detection is heuristic.** Users can set terminals to
  impersonate others (e.g. tmux inside iTerm). Conservative
  default (all capabilities off unless a confident signal
  present) is the right posture.
- **Future tmux passthrough.** Some tmux configs do pass OSC 8
  through via `allow-passthrough`. We don't detect this — users
  who care can set an env var override (`ANIE_FORCE_OSC8=1`).
  Document this in the comments.

## Exit criteria

- [ ] `TerminalCapabilities` available on `App`.
- [ ] Env-var detection matrix tested.
- [ ] No TUI behavior change yet (capabilities aren't consumed
      until Plan 05 wires them into markdown rendering).
- [ ] Environment variable override documented
      (`ANIE_FORCE_OSC8=1`, `ANIE_DISABLE_IMAGES=1`, etc.).

## Deferred

- **Inline image rendering.** Detecting support is cheap; actually
  rendering images (Kitty APC sequences, iTerm2 OSC 1337) is its
  own plan. Ship detection; gate image-emitting code behind the
  flag once an image renderer lands.
- **Dynamic DA/DA2 queries.** Not worth the event-stream
  complexity for the handful of capabilities we care about.
