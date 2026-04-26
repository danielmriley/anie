# 05 — Spinner: codex-style shimmer or blink fallback

## Rationale

Finding F-6. Anie's activity indicator
(`crates/anie-tui/src/app.rs:2151`) renders as a Braille-dot
spinner cycling through `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` next to "Responding…",
"Running tool…", or "compacting Ns".

User report: "the spinner needs work, but I'm not sure why."

Likely contributors:
- Braille glyphs render as rectangular blobs in some monospace
  fonts.
- The 10-frame cycle at ~10 fps feels stepped, not fluid.
- The accompanying static text ("Responding…") adds visual
  noise alongside the live indicator.

Codex's approach
(`codex-rs/tui/src/exec_cell/render.rs:183-197`):

```rust
pub(crate) fn spinner(start_time: Option<Instant>, animations_enabled: bool) -> Span<'static> {
    if !animations_enabled {
        return "•".dim();
    }
    if supports_color::on_cached(supports_color::Stream::Stdout)
        .map(|level| level.has_16m)
        .unwrap_or(false)
    {
        shimmer_spans("•")[0].clone()
    } else {
        let blink_on = (elapsed.as_millis() / 600).is_multiple_of(2);
        if blink_on { "•".into() } else { "◦".dim() }
    }
}
```

A single bullet character (`•`) that shimmers (color cycles
through subtle shades) on truecolor terminals, with a `•` ↔
`◦` blink fallback (600 ms period) on lower-color terminals.

Pi's spinner is similarly minimal — character count is one,
animation is in the styling not the glyph cycle.

## Design

### Step 1: add a `spinner` helper module to anie-tui

A small file `crates/anie-tui/src/spinner.rs` (or extend the
existing `Spinner` in `app.rs`) that:

```rust
pub fn spinner_glyph(elapsed: Duration, capabilities: &TerminalCapabilities) -> Span<'static> {
    if capabilities.true_color {
        shimmer_bullet(elapsed)
    } else {
        blink_bullet(elapsed)
    }
}

fn shimmer_bullet(elapsed: Duration) -> Span<'static> {
    // Color cycles through a small palette of yellows /
    // dim oranges. Codex uses HSL interpolation across a
    // ~1.5 s period.
    let phase = (elapsed.as_millis() % 1500) as f32 / 1500.0;
    let color = interpolate_hsl(phase);
    Span::styled("•", Style::default().fg(color))
}

fn blink_bullet(elapsed: Duration) -> Span<'static> {
    let on = (elapsed.as_millis() / 600) % 2 == 0;
    if on {
        Span::styled("•", Style::default().fg(Color::Yellow))
    } else {
        Span::styled("◦", Style::default().add_modifier(Modifier::DIM))
    }
}
```

Reuse the existing `TerminalCapabilities` shape — add a
`true_color: bool` field if not already there.

### Step 2: replace render_spinner_row glyph

`crates/anie-tui/src/app.rs:2143-2186`. Currently:

```rust
let text = match agent_state {
    AgentUiState::Streaming => format!("{spinner_frame} Responding..."),
    AgentUiState::ToolExecuting { tool_name } => {
        format!("{spinner_frame} Running {tool_name}...")
    }
    ...
};
```

Replace `{spinner_frame}` with the new shimmer/blink span and
drop the `…` suffix from "Responding" (the live spinner is the
"…"; the trailing dots are noise):

```rust
AgentUiState::Streaming => "Responding",
AgentUiState::ToolExecuting { tool_name } => format!("Running {tool_name}"),
```

…then prepend the live spinner span. The result is `• Responding`
or `• Running bash` with a live-animated bullet.

### Step 3: keep the same Idle clear-pattern

The Idle case still renders an empty paragraph to clear the
row. No change there.

### Step 4: braille glyph stays elsewhere

The existing `Spinner::frame()` cycle is also used as the
caller-side fallback inside `assistant_thinking_lines` and
similar — do **not** rip those out. Limit this PR to the
status-row use case. Each non-status-row caller should be
audited separately if it's ever a complaint.

## Files to touch

- `crates/anie-tui/src/spinner.rs` (new) — shimmer/blink
  helpers.
- `crates/anie-tui/src/app.rs` — `render_spinner_row` consumes
  the new glyph; drop trailing `…`.
- `crates/anie-tui/src/terminal_capabilities.rs` —
  `true_color: bool` if not present.
- Tests for the cycle math (deterministic given an `elapsed`
  Duration).

## Phased PRs

Single PR. Small.

## Test plan

1. **`shimmer_bullet_cycles_color_over_period`** — given a
   sequence of elapsed durations spanning 1500 ms, assert the
   color value differs (i.e., the shimmer is animating).
2. **`blink_bullet_alternates_on_600ms_period`** — at t=0,
   t=300, t=600, t=900, assert glyph alternates `•` / `◦`.
3. **`spinner_glyph_picks_shimmer_when_true_color_available`**
   — capability flag true → shimmer span; false → blink span.
4. **Manual smoke**: run anie under a truecolor terminal
   (kitty, iterm2) and a 256-color one (xterm). Verify
   shimmer vs blink looks right, neither is jarring.

## Risks

- **Subjective UX.** "It feels right" can take a few rounds.
  Plan to iterate. If shimmer feels too colorful, reduce the
  palette range. If blink feels too blinky, reduce the
  contrast between `•` and `◦`.
- **Color cycle frequency in renders.** The spinner span is
  re-built every render (~30 fps when streaming). Each call
  computes `elapsed`, modular math, and a Style — cheap, but
  worth confirming under bench.
- **Truecolor detection.** `supports_color::on_cached(...)`
  reads env vars and `tput`. Should be safe at startup. Cache
  the result.

## Exit criteria

- Spinner row renders the new `•` shimmer (or blink fallback)
  next to the activity label.
- "…" trailing dots removed from the activity strings.
- Manual smoke confirms the new look reads as live without
  feeling busy.
- `cargo test --workspace` green; clippy clean.
- No bench regression.

## Deferred

- Reworking other spinner sites (assistant_thinking_lines,
  tool block headers). They use the existing 10-frame braille
  cycle and aren't in the user complaint scope.
- Custom shimmer palettes per theme. Not until theme system
  exists.
- Replacing the bullet with a different shape entirely (e.g.,
  `◐◓◑◒`). Only revisit if the user dislikes `•`.

## A note on iteration

If the user looks at the new spinner and still says "needs
work," the next steps are usually:
- Reduce shimmer color range (less rainbow, more single-hue
  pulse).
- Slow or speed up the period.
- Try a different glyph.

Plan to do one round of iteration before declaring done.
