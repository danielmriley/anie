# Plan 02 — Synchronized output (DECSET 2026)

## Rationale

Modern GPU-accelerated terminals (Ghostty, Kitty, Alacritty,
WezTerm, Contour, modern tmux, Windows Terminal) render cells
at the GPU's VSync cadence. When ratatui's crossterm backend
emits a frame as many small writes, the terminal can paint
partial frames — a user sees a half-drawn transcript flicker
for a frame before the rest lands. This reads as "laggy" even
when the app is fast.

The DECSET 2026 "synchronized output" mode fixes this: wrap
the frame write sequence in
`\x1b[?2026h … \x1b[?2026l`, and the terminal buffers the
entire frame before painting it atomically. Terminals that
don't understand the escape sequence ignore it silently — zero
compatibility risk.

crossterm 0.27+ exposes this as `BeginSynchronizedUpdate` /
`EndSynchronizedUpdate`.

**This is the single lowest-cost, lowest-risk, highest-visual-
payoff change in the plan set.** It's scoped as its own plan
because it's tiny, independent, and can land in parallel with
the heavier Plan 03 work.

## Design

Wrap every `terminal.draw(...)` call site in anie-tui with
`BeginSynchronizedUpdate` / `EndSynchronizedUpdate`.

There's exactly one such call in the main loop
(`crates/anie-tui/src/app.rs:1481` per the audit) plus any
overlay/onboarding paths. A small helper keeps the wrap DRY.

```rust
// crates/anie-tui/src/terminal.rs  (helper)
pub fn draw_synchronized<B: Backend, F>(
    terminal: &mut Terminal<B>,
    f: F,
) -> io::Result<CompletedFrame>
where
    F: FnOnce(&mut Frame) -> (),
{
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    crossterm::execute!(io::stdout(), BeginSynchronizedUpdate)?;
    let frame = terminal.draw(f);
    crossterm::execute!(io::stdout(), EndSynchronizedUpdate)?;
    frame
}
```

All `terminal.draw(...)` sites switch to `draw_synchronized(...)`.

### References

- [crossterm `BeginSynchronizedUpdate`](https://docs.rs/crossterm/latest/crossterm/terminal/struct.BeginSynchronizedUpdate.html)
- [Synchronized output spec (Parpart, gist)](https://gist.github.com/christianparpart/d8a62cc1ab659194337d73e399004036)
- [Contour terminal docs on the extension](https://contour-terminal.org/vt-extensions/synchronized-output/)

## Files to touch

- `crates/anie-tui/src/terminal.rs`: add `draw_synchronized`
  helper.
- `crates/anie-tui/src/app.rs`: replace `terminal.draw(...)`
  with `draw_synchronized(&mut terminal, ...)` at the main
  loop site.
- Any overlay-owned draw sites (grep first — Agent D audit
  implies overlays dispatch through the main loop, but verify).
- `crates/anie-tui/src/tests.rs`: add a test using
  `TestBackend` that verifies the helper is transparent to
  ratatui's frame diff when the backend doesn't implement
  BSU/ESU.

## Phased PRs

Single PR. Too small to split.

## Test plan

- **Existing tests still pass.** `TestBackend` doesn't receive
  the BSU/ESU escape sequences because crossterm's
  `execute!(stdout(), ...)` writes to stdout directly, not to
  the backend. Verify no test captures stdout and fails on the
  new bytes.
- **Smoke: three terminals.** Before merging, run `anie` in
  Ghostty (GPU, BSU-supporting), gnome-terminal (no BSU), and
  tmux (proxy). All three should visually render correctly; GPU
  terminal should feel visibly smoother on long transcripts
  with streaming.
- **Unit: helper doesn't swallow errors.** If
  `BeginSynchronizedUpdate` fails (broken stdout), the error
  propagates. Test by feeding a `io::ErrorKind::BrokenPipe`
  writer and asserting the error surfaces.

## Risks

- **None substantive.** BSU/ESU is a terminal hint. Unsupported
  terminals ignore it.
- **Minor: mis-nested BSU.** If we accidentally call
  `terminal.draw` without the wrapper elsewhere, nothing
  breaks — it just doesn't benefit. Grep for every `draw(` in
  anie-tui/src to confirm all paths are wrapped.
- **Terminals with a buggy implementation** (historically
  some older tmux versions) may hold the frame too long.
  Mitigation: env-var escape hatch `ANIE_DISABLE_SYNC_OUTPUT=1`
  that bypasses the helper. Cheap insurance.

## Exit criteria

- [ ] Every `terminal.draw(...)` site in anie-tui uses the
      helper.
- [ ] `ANIE_DISABLE_SYNC_OUTPUT=1` env var disables the wrap.
- [ ] Manual smoke on Ghostty + gnome-terminal + tmux —
      subjective "no tearing, feels smoother" on Ghostty;
      unchanged on the others.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
      clean.

## Deferred

- **Querying the terminal for BSU support** via DA-1 / DA-3
  and only emitting the sequence when supported. Not worth it
  — unsupported terminals ignore the bytes, and querying adds
  handshake latency.
- **BSU on input read** (for paste / bracketed-paste).
  Separate problem; this plan is output-only.
