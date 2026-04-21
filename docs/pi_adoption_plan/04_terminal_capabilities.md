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

A `TerminalCapabilities` struct populated once at startup:

```rust
pub struct TerminalCapabilities {
    /// Terminal emulator family, detected from env vars like
    /// `TERM_PROGRAM`, `TERM`, `KITTY_WINDOW_ID`, `WEZTERM_EXECUTABLE`.
    pub emulator: TerminalEmulator,
    /// Whether we're running under tmux or screen. Those
    /// multiplexers swallow most OSC sequences unless
    /// passthrough is enabled — we default to "off" to be safe.
    pub inside_multiplexer: bool,
    /// Whether stdout is a TTY. False in pipe / file redirects.
    pub is_tty: bool,
    /// Whether the terminal supports OSC 8 hyperlinks.
    /// Derived: `is_tty && !inside_multiplexer &&
    /// emulator_supports_osc8`.
    pub supports_osc8_hyperlinks: bool,
    /// Whether the terminal supports Kitty image protocol.
    pub supports_kitty_images: bool,
    /// Whether the terminal supports iTerm2 image protocol.
    pub supports_iterm_images: bool,
    /// Whether truecolor (24-bit) is advertised.
    pub supports_truecolor: bool,
}

pub enum TerminalEmulator {
    Kitty,
    Ghostty,
    WezTerm,
    Iterm2,
    VsCode,
    Alacritty,
    AppleTerminal,
    Windows,
    Unknown,
}
```

Detection strategy mirrors pi's approach — env vars first, then
fallbacks. No dynamic queries (the terminal DA/DA2 query protocol
is awkward under crossterm's event stream and not worth the
complexity for capabilities we can infer statically).

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
