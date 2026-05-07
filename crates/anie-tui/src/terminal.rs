use std::io::{self, BufWriter, Stdout, Write, stdout};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::{
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, ClearType, CrosstermBackend, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
};

/// Enable button + scroll mouse reporting only.
///
/// Crossterm's `EnableMouseCapture` enables `?1003h` (any-event
/// motion tracking), which makes the terminal forward an event
/// for **every** mouse cursor movement over the window. We
/// only use clicks (URL hit-test) and scroll wheel; motion
/// events are pure noise that wake the event loop, drain
/// keystroke batches alongside, and burn CPU on a cursor that
/// happens to drift across the anie window. Worse, motion
/// tracking adds visible latency to typing on a busy system —
/// the user moves the mouse out of the way to focus on the
/// keyboard, which streams motion events that compete with
/// keystrokes for the same dispatch path.
///
/// `?1000h` reports button presses and releases (covers clicks
/// AND scroll wheel as button 4 / button 5). `?1006h` is SGR
/// extended encoding so column / row > 223 are reported
/// correctly. That's everything anie needs; `?1002h` (drag
/// motion) and `?1003h` (any motion) are deliberately
/// omitted.
///
/// As a happy side effect, dropping motion tracking also lets
/// users select text natively in their terminal during a
/// live anie session — without `?1003`, click-and-drag is no
/// longer captured by the program when no button is held, so
/// the terminal's own selection takes over for prose.
const ENABLE_BUTTON_ONLY_MOUSE: &str = concat!(
    "\x1b[?1000h", // Normal tracking: send mouse X/Y on button press + release
    "\x1b[?1006h", // SGR mouse mode: required for coordinates > 223
);

/// Inverse of `ENABLE_BUTTON_ONLY_MOUSE`.
const DISABLE_BUTTON_ONLY_MOUSE: &str = concat!("\x1b[?1006l", "\x1b[?1000l",);

/// Capacity of the stdout buffer wrapping `CrosstermBackend`'s
/// writer. ratatui issues many small `queue!` writes per frame
/// (one per cell update plus cursor / color / modifier changes);
/// without buffering, each one acquires the global stdout lock
/// individually, which dominates real-world keystroke latency
/// on a busy terminal. 64 KiB comfortably holds a full
/// fullscreen frame's diff worth of escape sequences (a 200×80
/// terminal at worst writes ~16 K cells × ~25 B/cell ≈ 400 KiB
/// for a *complete* repaint; typical keystroke diffs are <100 B,
/// streaming diffs <10 KiB). Single allocation per process; the
/// memory cost is negligible vs. the syscall savings. See the
/// ratatui FAQ — "Should I use stdout or stderr?" — for the
/// upstream recommendation.
const STDOUT_BUFFER_BYTES: usize = 64 * 1024;

/// The buffered stdout writer wrapped by the ratatui backend.
/// Aliased so the verbose nested type doesn't appear in every
/// public signature that hands the terminal off — `run_tui`,
/// `terminal_mut`, etc.
///
/// Layer order (writes flow top → bottom):
/// `CrosstermBackend → DeferredFlushBufWriter → BufWriter
///   → CountingWriter → Stdout`
///
/// [`DeferredFlushBufWriter`] absorbs the intermediate
/// `flush` calls ratatui issues mid-frame (one per
/// `execute!(Show)`, one per `execute!(MoveTo)`, plus the
/// trailing `backend.flush`). With those suppressed during a
/// frame, the inner [`BufWriter`] accumulates the entire
/// frame's bytes and one explicit flush after `terminal.draw`
/// returns produces a single `write(2)` syscall.
pub type TerminalStdout = DeferredFlushBufWriter;

/// Optional bytes-per-flush counter for the terminal write
/// pipeline. Wraps the inner [`Stdout`] so that each
/// [`BufWriter::flush`] (which fires at the end of every
/// `terminal.draw` cycle) corresponds to one
/// [`CountingWriter::flush`] here.
///
/// Three opt-in modes via env vars (each independent):
/// - `ANIE_TRACE_FLUSH=1` — emit a tracing event with the
///   byte count per frame.
/// - `ANIE_TRACE_FLUSH_BYTES=1` — additionally accumulate the
///   actual bytes and log them as an escape-encoded string,
///   so we can read off precisely which sequences ratatui +
///   our render emitted that frame. Use to compare against
///   `vim`'s per-keystroke output and identify VTE-slow
///   sequences. Implies `ANIE_TRACE_FLUSH`.
///
/// Off by default — the only steady-state cost is the env-var
/// read at construction; the per-write hot path is then a
/// branch that does nothing.
///
/// Diagnostic for confirming or refuting code-side estimates of
/// per-frame escape-sequence volume — see rounds 4 and 6 of
/// `docs/code_review_2026-05-03.md`.
pub struct CountingWriter<W: Write> {
    inner: W,
    bytes_in_flush: u64,
    /// Frame's bytes accumulated when `dump_bytes` is on.
    /// Cleared on every flush. Capacity is held across flushes
    /// so steady-state typing doesn't reallocate.
    dump_buffer: Vec<u8>,
    enabled: bool,
    /// True when `ANIE_TRACE_FLUSH_BYTES=1`. Implies `enabled`.
    dump_bytes: bool,
}

impl<W: Write> CountingWriter<W> {
    fn new(inner: W) -> Self {
        let dump_bytes = std::env::var("ANIE_TRACE_FLUSH_BYTES")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        let enabled = dump_bytes
            || std::env::var("ANIE_TRACE_FLUSH")
                .map(|v| !v.is_empty() && v != "0")
                .unwrap_or(false);
        Self {
            inner,
            bytes_in_flush: 0,
            dump_buffer: Vec::new(),
            enabled,
            dump_bytes,
        }
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        if self.enabled {
            self.bytes_in_flush = self.bytes_in_flush.saturating_add(n as u64);
            if self.dump_bytes {
                self.dump_buffer.extend_from_slice(&buf[..n]);
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.inner.flush();
        if self.enabled && self.bytes_in_flush > 0 {
            if self.dump_bytes {
                let escaped = escape_for_log(&self.dump_buffer);
                tracing::info!(
                    target: "anie_tui::flush_bytes",
                    bytes = self.bytes_in_flush,
                    data = %escaped,
                    "frame flush",
                );
                self.dump_buffer.clear();
            } else {
                tracing::info!(
                    target: "anie_tui::flush_bytes",
                    bytes = self.bytes_in_flush,
                    "frame flush",
                );
            }
            self.bytes_in_flush = 0;
        }
        result
    }
}

/// Escape a byte slice for human-readable logging. ESC →
/// `\e`, common control chars get short forms (`\n`, `\r`,
/// `\t`), printable ASCII is passed through, everything else
/// becomes `\xNN`. Mirrors the convention shells use for
/// `printf %q` so log readers can copy-paste the sequence
/// back to verify what a terminal would interpret.
fn escape_for_log(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        match b {
            0x1b => out.push_str("\\e"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Process-wide flag used by [`DeferredFlushBufWriter`] to
/// decide whether `flush` is a real flush or a no-op. Set to
/// `true` for the duration of one [`Terminal::draw`] /
/// `draw_synchronized` call so the intermediate `execute!`
/// flushes ratatui issues mid-frame (one for `Show`, one for
/// `MoveTo`, plus the trailing `backend.flush`) all land in
/// the same `BufWriter` and a single explicit flush after the
/// draw produces one `write(2)` syscall.
///
/// Single-threaded by construction — the run loop calls
/// `terminal.draw` from one tokio task; cleanup writes from
/// `TerminalGuard::restore` and the panic hook either run on
/// that same task or use a fresh `Stdout` outside this
/// pipeline (panic hook). `Relaxed` ordering is sufficient
/// because there is no cross-thread synchronization to
/// coordinate.
static DEFER_FLUSH: AtomicBool = AtomicBool::new(false);

/// RAII helper that sets [`DEFER_FLUSH`] for the lifetime of
/// one frame. Clearing the flag on `Drop` (rather than after
/// the closure returns) guarantees a panic mid-draw can't
/// leave the writer permanently in deferred-flush mode.
pub(crate) struct DeferFlushGuard;

impl DeferFlushGuard {
    pub(crate) fn begin() -> Self {
        DEFER_FLUSH.store(true, Ordering::Relaxed);
        Self
    }
}

impl Drop for DeferFlushGuard {
    fn drop(&mut self) {
        DEFER_FLUSH.store(false, Ordering::Relaxed);
    }
}

/// Writer layer that absorbs intermediate `flush` calls during
/// a frame, leaving the inner [`BufWriter`] to accumulate the
/// whole frame's bytes. ratatui's `Terminal::draw` issues two
/// `execute!`-driven flushes per frame (`show_cursor` +
/// `set_cursor_position`) on top of the trailing
/// `backend.flush` — three syscalls per keystroke. Some
/// terminals (notably Gnome Terminal's VTE) refresh their
/// renderer between input chunks, which surfaces the cursor
/// reposition one display frame after the typed character —
/// the cursor-lag-while-typing the user reports. With this
/// layer, the run loop sets [`DEFER_FLUSH`] for the duration
/// of one paint and explicitly flushes after the draw,
/// collapsing the frame into a single `write(2)`.
///
/// A previous round (round 8 of
/// `docs/code_review_2026-05-03.md`) experimented with
/// post-processing the buffered bytes to collapse
/// `\e[39m\e[49m\e[0m` → `\e[0m` (saving 8 B/frame). The
/// trace data confirmed the saving but real-terminal feel
/// regressed. Reverted: bytes-volume past ~22 B/keystroke is
/// not the controlling variable for felt typing latency, so
/// further byte-trimming on this layer isn't worth the risk.
pub struct DeferredFlushBufWriter {
    inner: BufWriter<CountingWriter<Stdout>>,
}

impl DeferredFlushBufWriter {
    fn new(inner: BufWriter<CountingWriter<Stdout>>) -> Self {
        Self { inner }
    }
}

impl Write for DeferredFlushBufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        if DEFER_FLUSH.load(Ordering::Relaxed) {
            // Mid-frame `execute!` from ratatui — let the
            // bytes accumulate in the inner `BufWriter` until
            // the run loop's explicit flush after
            // `terminal.draw` returns.
            Ok(())
        } else {
            self.inner.flush()
        }
    }
}

/// Suppresses backend operations that would emit redundant
/// escape sequences. ratatui's [`Terminal::draw`] emits
/// `\e[?25h` (show cursor) every frame whether the cursor was
/// already shown or not, and `\e[<r>;<c>H` (cursor reposition)
/// whether or not the cursor already lands at that position
/// from the natural advance after writing the typed cell.
/// Round 7 of `docs/code_review_2026-05-03.md`: real-terminal
/// byte-dump (`ANIE_TRACE_FLUSH_BYTES=1`) showed those two
/// emissions account for ~13 of the 40 bytes in a typing-hot
/// keystroke frame. With them suppressed, a typed-character
/// frame drops to ~27 bytes — still ratatui-shaped (the
/// cell-write cursor move is structural to the diff model)
/// but a ~33 % byte reduction on the path the user reports as
/// stuttery.
///
/// State tracked:
/// - `cursor_shown`: starts `false`, set true on the first
///   forwarded `show_cursor`. Cursor visibility never decays
///   silently — only `hide_cursor` clears it.
/// - `last_known_cursor`: post-condition position of the most
///   recent operation that left the cursor at a known spot.
///   Cleared on operations whose cursor effect is hard to
///   model precisely (`clear`, `clear_region`, `append_lines`)
///   so the next `set_cursor_position` correctly fires
///   instead of being suppressed against a stale value.
///
/// `draw` updates `last_known_cursor` to the position right
/// after the last cell written, treating each cell as
/// 1-column-wide. Wide grapheme clusters (CJK, emoji) will
/// leave the tracker off by one for those edges, but the only
/// consequence is that the next `set_cursor_position` fires
/// instead of being suppressed — strictly correct, just one
/// frame's worth of "missed" optimization.
pub struct RedundancySuppressor<B: Backend + io::Write> {
    inner: B,
    cursor_shown: bool,
    last_known_cursor: Option<Position>,
}

impl<B: Backend + io::Write> RedundancySuppressor<B> {
    fn new(inner: B) -> Self {
        Self {
            inner,
            cursor_shown: false,
            last_known_cursor: None,
        }
    }
}

impl<B: Backend + io::Write> Backend for RedundancySuppressor<B> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Track the last cell coordinate the inner backend
        // wrote. After `inner.draw`, the terminal cursor sits
        // one column to the right of that cell (assuming
        // 1-wide grapheme — see struct doc). We update
        // `last_known_cursor` to that position so a follow-up
        // `set_cursor_position` to the same place can be
        // suppressed.
        //
        // `inspect` runs the closure synchronously inside
        // `inner.draw`'s consume of the iterator, so the
        // borrow of `last` ends before this method returns.
        let mut last: Option<Position> = None;
        self.inner.draw(content.inspect(|(x, y, _cell)| {
            last = Some(Position::new(x.saturating_add(1), *y));
        }))?;
        if let Some(p) = last {
            self.last_known_cursor = Some(p);
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        if self.cursor_shown {
            return Ok(());
        }
        self.inner.show_cursor()?;
        self.cursor_shown = true;
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()?;
        self.cursor_shown = false;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        if let Some(p) = self.last_known_cursor {
            return Ok(p);
        }
        let p = self.inner.get_cursor_position()?;
        self.last_known_cursor = Some(p);
        Ok(p)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let p = position.into();
        if Some(p) == self.last_known_cursor {
            return Ok(());
        }
        self.inner.set_cursor_position(p)?;
        self.last_known_cursor = Some(p);
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()?;
        // Cursor position is implementation-defined after a
        // clear; drop our tracking so the next reposition is
        // emitted unconditionally.
        self.last_known_cursor = None;
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)?;
        self.last_known_cursor = None;
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)?;
        // Lines pushed up; column may stay but row tracking
        // is no longer reliable.
        self.last_known_cursor = None;
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

impl<B: Backend + io::Write> io::Write for RedundancySuppressor<B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.inner)
    }
}

/// The full backend chain ratatui's `Terminal` is
/// parameterised over: redundancy suppression on top of
/// `CrosstermBackend`, which itself writes through
/// `DeferredFlushBufWriter` → `BufWriter` → `CountingWriter`
/// → `Stdout`.
pub type TerminalBackend = RedundancySuppressor<CrosstermBackend<TerminalStdout>>;

/// RAII guard around the TUI terminal setup.
///
/// Holds the configured `Terminal` and guarantees that raw mode is
/// disabled, the alternate screen is left, and mouse capture is
/// turned off whenever this value is dropped. The Drop path
/// catches the messy failure modes the explicit `restore` call
/// misses:
///
/// - an error inside `run_tui` that bubbles out before the caller
///   reaches `guard.restore()`;
/// - a panic anywhere on the TUI path (`Drop` runs during
///   stack unwinding);
/// - a `?` early return from any code path that owns the guard.
///
/// Without this, a panic or early-return would leave the terminal
/// in SGR mouse-tracking + raw + alternate-screen mode. The shell
/// then prints raw mouse-event escape sequences (e.g.
/// `\x1b[<0;51;57M`) every time the user clicks or scrolls — the
/// string-fragments that show up after a crash.
///
/// Signal-killed processes (SIGKILL, SIGQUIT) can't run Drop and
/// aren't covered. The shutdown-signal forwarder handles SIGTERM /
/// SIGHUP via a normal Quit action, which then drops the guard
/// cleanly.
pub struct TerminalGuard {
    terminal: Terminal<TerminalBackend>,
    // Once `restore` has run (either explicitly or via Drop), we
    // must not issue the terminal commands a second time —
    // repeated `LeaveAlternateScreen` etc. are idempotent but we
    // skip them to avoid stray errors during unwinding.
    restored: bool,
}

impl TerminalGuard {
    /// Enter raw mode + alternate screen + mouse capture and
    /// return a guard that owns the configured terminal.
    ///
    /// The backend writer is wrapped in `BufWriter` so that the
    /// thousands of small `queue!` writes ratatui emits per
    /// frame coalesce into one (or a small handful of) syscalls
    /// instead of hammering the global stdout lock per cell.
    /// This is the upstream-recommended pattern from ratatui's
    /// FAQ. The buffer is flushed by ratatui at the end of every
    /// `terminal.draw(...)` (via `Backend::flush`), so behavior
    /// is observationally identical — only the syscall rate
    /// changes.
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        // Write the alternate-screen enable sequence directly to
        // the unbuffered handle so it lands before we hand stdout
        // off to the buffered backend. Then the button-only mouse
        // sequence — see `ENABLE_BUTTON_ONLY_MOUSE` for why we
        // bypass crossterm's `EnableMouseCapture` here. Both must
        // flush before the `BufWriter` installs, so there's
        // nothing competing with the buffered handle.
        execute!(stdout, EnterAlternateScreen)?;
        stdout.write_all(ENABLE_BUTTON_ONLY_MOUSE.as_bytes())?;
        stdout.flush()?;
        let counting = CountingWriter::new(stdout);
        let buf_writer = BufWriter::with_capacity(STDOUT_BUFFER_BYTES, counting);
        let writer = DeferredFlushBufWriter::new(buf_writer);
        let backend = RedundancySuppressor::new(CrosstermBackend::new(writer));
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    /// Borrow the underlying terminal for rendering.
    pub fn terminal_mut(&mut self) -> &mut Terminal<TerminalBackend> {
        &mut self.terminal
    }

    /// Explicit restore, preserving error reporting for the
    /// caller. Subsequent `Drop` is a no-op. Safe to call more
    /// than once.
    pub fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        self.restored = true;
        disable_raw_mode()?;
        // Disable button-only mouse reporting first (mirror of
        // enable order in `new`), then leave alternate screen.
        // Both go through the buffered backend; ratatui flushes
        // on `execute!`.
        let backend = self.terminal.backend_mut();
        backend.write_all(DISABLE_BUTTON_ONLY_MOUSE.as_bytes())?;
        execute!(backend, LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort — any error during unwind is swallowed. If
        // the alternate-screen leave or mouse-capture disable
        // fails here, there's nothing useful the panic handler
        // could do with it. Both commands are idempotent, so
        // overlap with an earlier explicit `restore` is safe.
        if self.restored {
            return;
        }
        self.restored = true;
        let _ = disable_raw_mode();
        let backend = self.terminal.backend_mut();
        let _ = backend.write_all(DISABLE_BUTTON_ONLY_MOUSE.as_bytes());
        let _ = execute!(backend, LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Set up the terminal for TUI rendering.
///
/// Returns a `TerminalGuard` that will automatically restore
/// terminal state when dropped — no matter how the function that
/// owns the guard exits.
pub fn setup_terminal() -> Result<TerminalGuard> {
    TerminalGuard::new()
}

/// Explicit restore; prefer dropping the guard instead.
///
/// Kept for callers that want to surface the restore error
/// immediately rather than rely on Drop's best-effort cleanup. If
/// you call this, the subsequent `Drop` is a no-op.
pub fn restore_terminal(guard: &mut TerminalGuard) -> Result<()> {
    guard.restore()
}

/// Draw a single frame wrapped in DECSET 2026 synchronized
/// output (`\x1b[?2026h` … `\x1b[?2026l`). Supported by
/// modern GPU-backed terminals (Ghostty, Kitty, Alacritty,
/// WezTerm, Contour, current tmux, Windows Terminal) and
/// ignored silently by terminals that don't understand it.
///
/// The payoff is visual: the terminal buffers the whole
/// frame before compositing, so long transcripts never
/// tear mid-frame. Terminals that ignore it see exactly the
/// same behavior as a bare `terminal.draw(...)`.
///
/// Set `ANIE_DISABLE_SYNC_OUTPUT=1` to bypass the wrap if a
/// buggy-sync terminal shows up in the wild. Read once per
/// process; flipping the env var requires a restart.
///
/// Errors on `Begin`/`End` are forwarded — if the terminal
/// write has failed at this point, the frame itself is
/// already broken and surfacing it loudly is correct.
pub fn draw_synchronized<B, F>(terminal: &mut Terminal<B>, render_callback: F) -> io::Result<()>
where
    B: Backend + io::Write,
    F: FnOnce(&mut Frame),
{
    if sync_output_disabled() {
        return draw_urgent(terminal, render_callback);
    }
    // The guard suppresses every flush issued during this
    // function — both ratatui's internal `execute!`s in
    // `Terminal::draw` (Show / MoveTo / trailing flush) and
    // our own BSU/ESU `execute!`s. After the guard drops, an
    // explicit `backend.flush()` commits the whole frame in
    // one syscall.
    let guard = DeferFlushGuard::begin();
    // Failures on Begin are surfaced — a downstream draw call
    // that's also going to fail is strictly worse than reporting
    // the earlier failure.
    let bsu_result = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
    let draw_result = terminal.draw(render_callback).map(|_| ());
    // End runs regardless of whether draw succeeded so a failed
    // frame doesn't leave the terminal in synchronized-buffering
    // mode forever. draw's error takes precedence if present.
    let end_result = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    drop(guard);
    let flush_result = Backend::flush(terminal.backend_mut());
    bsu_result?;
    draw_result?;
    end_result?;
    flush_result?;
    Ok(())
}

/// Draw a single frame WITHOUT the DECSET 2026 wrap —
/// intended for keystroke-driven paints where input
/// latency matters more than tearing avoidance. A single
/// keystroke changes only a handful of cells (the typed
/// char plus the cursor position); tearing that the sync
/// wrap prevents isn't perceptible on that scale, and
/// skipping BSU/ESU saves a terminal round-trip (on
/// sync-capable terminals, it also skips a VSync-alignment
/// wait that can add 8-16 ms).
///
/// Callers should use this only when they'd prefer lowest
/// latency over atomic composition. Streaming paints,
/// scroll redraws, and resize-final paints still want
/// `draw_synchronized`.
pub fn draw_urgent<B, F>(terminal: &mut Terminal<B>, render_callback: F) -> io::Result<()>
where
    B: Backend + io::Write,
    F: FnOnce(&mut Frame),
{
    // Defer the two intermediate `execute!` flushes ratatui
    // emits inside `Terminal::draw` (Show + MoveTo) so the
    // whole frame coalesces into a single `write(2)` after
    // the guard drops. Drop the guard *before* the explicit
    // flush so the flush itself isn't suppressed.
    let guard = DeferFlushGuard::begin();
    // Map to `()` immediately so the `CompletedFrame`
    // borrow on `terminal` is released before the explicit
    // flush below.
    let draw_result = terminal.draw(render_callback).map(|_| ());
    drop(guard);
    let flush_result = Backend::flush(terminal.backend_mut());
    draw_result?;
    flush_result?;
    Ok(())
}

/// Read-once toggle for the synchronized-output wrap. See
/// `draw_synchronized` for the rationale.
fn sync_output_disabled() -> bool {
    static DISABLED: OnceLock<bool> = OnceLock::new();
    *DISABLED.get_or_init(|| {
        std::env::var("ANIE_DISABLE_SYNC_OUTPUT")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Install a panic hook that attempts to restore terminal state first.
///
/// Duplicates a subset of `TerminalGuard::drop` so a panic while
/// the guard isn't yet in scope (extremely unlikely given
/// `setup_terminal` is the first thing we call) or during a
/// double-fault scenario still leaves a usable terminal. Best
/// effort — Drop is the primary cleanup path.
pub fn install_panic_hook() {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let mut out = stdout();
        let _ = out.write_all(DISABLE_BUTTON_ONLY_MOUSE.as_bytes());
        let _ = execute!(out, LeaveAlternateScreen);
        original_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::CrosstermBackend, widgets::Paragraph};
    use std::sync::{Arc, Mutex};

    /// Write adapter backed by an `Arc<Mutex<Vec<u8>>>` so the
    /// test can inspect emitted bytes after the terminal has
    /// finished writing. ratatui 0.29's `CrosstermBackend::writer`
    /// is gated behind an unstable feature, so we own the buffer
    /// on our side.
    #[derive(Clone)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CapturedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("captured writer lock"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn captured_backend() -> (Arc<Mutex<Vec<u8>>>, CrosstermBackend<CapturedWriter>) {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CapturedWriter(buf.clone());
        (buf, CrosstermBackend::new(writer))
    }

    /// BSU/ESU is applied around the draw when
    /// `ANIE_DISABLE_SYNC_OUTPUT` is unset (the default). The
    /// captured buffer must contain `\x1b[?2026h` before the
    /// frame content and `\x1b[?2026l` after it.
    #[test]
    fn draw_synchronized_wraps_frame_in_decset_2026() {
        let (captured, backend) = captured_backend();
        let mut terminal = Terminal::new(backend).expect("terminal");
        draw_synchronized(&mut terminal, |f| {
            let area = f.area();
            f.render_widget(Paragraph::new("hi"), area);
        })
        .expect("draw");

        let buf = captured.lock().expect("lock").clone();
        assert!(
            buf.windows(8).any(|w| w == b"\x1b[?2026h"),
            "BSU escape sequence missing from output"
        );
        assert!(
            buf.windows(8).any(|w| w == b"\x1b[?2026l"),
            "ESU escape sequence missing from output"
        );
        let bsu_idx = buf
            .windows(8)
            .position(|w| w == b"\x1b[?2026h")
            .expect("bsu");
        let esu_idx = buf
            .windows(8)
            .position(|w| w == b"\x1b[?2026l")
            .expect("esu");
        assert!(bsu_idx < esu_idx, "ESU must follow BSU");
    }

    /// Mouse-capture sequence pin. The constants must enable
    /// only button events (`?1000h`) + SGR encoding (`?1006h`)
    /// — never `?1002h` (drag motion) or `?1003h` (any-event
    /// motion). Motion tracking generates a constant stream
    /// of events as the user's mouse drifts across the
    /// terminal, which competes with keystrokes in our event
    /// loop. The matching disable sequence must invert in
    /// reverse order so the terminal returns to the same
    /// state we found it in.
    #[test]
    fn mouse_capture_sequences_omit_motion_tracking() {
        assert!(
            !ENABLE_BUTTON_ONLY_MOUSE.contains("?1002"),
            "drag-motion tracking must not be enabled"
        );
        assert!(
            !ENABLE_BUTTON_ONLY_MOUSE.contains("?1003"),
            "any-event motion tracking must not be enabled"
        );
        assert!(
            ENABLE_BUTTON_ONLY_MOUSE.contains("?1000h"),
            "button-press tracking must be enabled (covers click + scroll wheel)"
        );
        assert!(
            ENABLE_BUTTON_ONLY_MOUSE.contains("?1006h"),
            "SGR encoding must be enabled so coords > 223 report correctly"
        );
        // Disable mirrors enable, in reverse order.
        assert!(
            DISABLE_BUTTON_ONLY_MOUSE.contains("?1000l"),
            "disable must clear ?1000"
        );
        assert!(
            DISABLE_BUTTON_ONLY_MOUSE.contains("?1006l"),
            "disable must clear ?1006"
        );
        let disable_idx = DISABLE_BUTTON_ONLY_MOUSE
            .find("?1000l")
            .expect("?1000l in disable");
        let sgr_idx = DISABLE_BUTTON_ONLY_MOUSE
            .find("?1006l")
            .expect("?1006l in disable");
        assert!(
            sgr_idx < disable_idx,
            "?1006l must come before ?1000l (mirror of enable order)"
        );
    }

    /// Regression: the frame's own render output must still
    /// land between the BSU and ESU markers. A caller swapping
    /// `terminal.draw` for `draw_synchronized` must see the
    /// same pixels.
    #[test]
    fn draw_synchronized_still_writes_frame_content() {
        let (captured, backend) = captured_backend();
        let mut terminal = Terminal::new(backend).expect("terminal");
        draw_synchronized(&mut terminal, |f| {
            let area = f.area();
            f.render_widget(Paragraph::new("xyzz"), area);
        })
        .expect("draw");

        let buf = captured.lock().expect("lock").clone();
        assert!(
            buf.windows(4).any(|w| w == b"xyzz"),
            "frame content missing from synchronized-output write stream"
        );
    }

    /// `CountingWriter` accumulates bytes across multiple
    /// `write` calls and reports the running total to the
    /// `bytes_in_flush` field; `flush` resets the counter so
    /// the next frame starts at zero. The tracing emission is
    /// covered indirectly — gating the log on `enabled` plus
    /// the counter check is the only behavior beyond
    /// forward-and-tally that the production path relies on.
    #[test]
    fn counting_writer_tracks_bytes_per_flush_when_enabled() {
        let mut cw = CountingWriter {
            inner: Vec::<u8>::new(),
            bytes_in_flush: 0,
            dump_buffer: Vec::new(),
            enabled: true,
            dump_bytes: false,
        };
        cw.write_all(b"hello").expect("write 1");
        cw.write_all(b" world").expect("write 2");
        assert_eq!(cw.bytes_in_flush, 11);
        cw.flush().expect("flush");
        assert_eq!(cw.bytes_in_flush, 0);
        // A second cycle should observe the new bytes only,
        // proving the reset on flush.
        cw.write_all(b"xyz").expect("write 3");
        assert_eq!(cw.bytes_in_flush, 3);
    }

    /// `dump_bytes` mode also accumulates the raw frame
    /// bytes; flushing clears the buffer so the next frame
    /// starts fresh. The capacity is reused across flushes —
    /// pin that by checking `Vec::capacity` doesn't drop to 0
    /// after a clear.
    #[test]
    fn counting_writer_accumulates_bytes_for_dump_when_enabled() {
        let mut cw = CountingWriter {
            inner: Vec::<u8>::new(),
            bytes_in_flush: 0,
            dump_buffer: Vec::new(),
            enabled: true,
            dump_bytes: true,
        };
        cw.write_all(b"\x1b[37mx").expect("write");
        assert_eq!(cw.dump_buffer, b"\x1b[37mx");
        let cap = cw.dump_buffer.capacity();
        cw.flush().expect("flush");
        assert!(cw.dump_buffer.is_empty(), "dump cleared on flush");
        assert!(
            cw.dump_buffer.capacity() >= cap,
            "capacity preserved across flushes for steady-state typing"
        );
    }

    /// `escape_for_log` is the readability layer between the
    /// raw bytes and the tracing log line. ESC must come out
    /// as `\e`, printable ASCII passes through, control bytes
    /// get short escapes, anything else is `\xNN`.
    #[test]
    fn escape_for_log_round_trips_common_terminal_sequences() {
        // A real keystroke frame fragment: cursor move, set
        // color, the char itself, SGR resets, show cursor.
        let bytes = b"\x1b[5;3H\x1b[37mx\x1b[39m\x1b[?25h";
        let s = escape_for_log(bytes);
        assert_eq!(s, "\\e[5;3H\\e[37mx\\e[39m\\e[?25h");
        // Edge cases: tab + newline + non-printable byte.
        assert_eq!(escape_for_log(b"\t\n\x80"), "\\t\\n\\x80");
        // Backslash itself is escaped so the output is
        // unambiguous when copy-pasted back.
        assert_eq!(escape_for_log(b"a\\b"), "a\\\\b");
    }

    /// Disabled mode (production default) does no
    /// accounting — the counter never increments and flush is
    /// a pure forward.
    #[test]
    fn counting_writer_is_inert_when_disabled() {
        let mut cw = CountingWriter {
            inner: Vec::<u8>::new(),
            bytes_in_flush: 0,
            dump_buffer: Vec::new(),
            enabled: false,
            dump_bytes: false,
        };
        cw.write_all(b"data").expect("write");
        assert_eq!(cw.bytes_in_flush, 0);
        cw.flush().expect("flush");
        assert_eq!(cw.bytes_in_flush, 0);
    }

    /// A writer adapter we control so the test can inspect
    /// every `flush` call. ratatui's `Terminal::draw` and our
    /// own `execute!` calls all eventually bottom out in
    /// `Write::flush` — the regression we're guarding against
    /// is that those mid-frame flushes propagate through to
    /// the inner writer when `DeferFlushGuard` is active.
    struct FlushCounter {
        flush_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl io::Write for FlushCounter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flush_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    /// Test-only stand-in for `DeferredFlushBufWriter` over an
    /// arbitrary inner writer. Production code is restricted
    /// to `CountingWriter<Stdout>` for type-system reasons
    /// (the `TerminalStdout` alias and ratatui's `Terminal`
    /// generic), but the deferred-flush behaviour itself is
    /// generic over the inner type. This shim mirrors the
    /// production layer's flush logic so the test can point a
    /// `FlushCounter` at the guard.
    struct DeferredFlushTest<W: io::Write> {
        inner: W,
    }

    impl<W: io::Write> io::Write for DeferredFlushTest<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            if DEFER_FLUSH.load(Ordering::Relaxed) {
                Ok(())
            } else {
                self.inner.flush()
            }
        }
    }

    /// `DeferFlushGuard` toggles `DEFER_FLUSH` on construction
    /// and clears it on drop. Mid-frame `flush` calls land on
    /// the deferred-flush layer, which short-circuits when the
    /// flag is set. The contract under test is "guard active
    /// → inner flush is suppressed; guard dropped → inner
    /// flush propagates."
    ///
    /// Reset/restore the global flag so cargo's parallel test
    /// harness doesn't see leaked state from a panicking sibling
    /// — `DeferFlushGuard::begin` + drop accomplishes that.
    #[test]
    fn defer_flush_guard_suppresses_inner_flush_until_drop() {
        // Start clean — paranoid against a previous test that
        // panicked before its guard dropped.
        DEFER_FLUSH.store(false, Ordering::Relaxed);

        let count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut writer = DeferredFlushTest {
            inner: FlushCounter {
                flush_calls: std::sync::Arc::clone(&count),
            },
        };

        // Guard active: flushes against `writer` short-circuit
        // — the FlushCounter's flush is never reached.
        {
            let _guard = DeferFlushGuard::begin();
            writer.write_all(b"frame data").expect("write");
            writer.flush().expect("flush 1");
            writer.flush().expect("flush 2");
            assert_eq!(
                count.load(std::sync::atomic::Ordering::Relaxed),
                0,
                "guard active: inner flushes must be suppressed",
            );
        }

        // Guard dropped: subsequent flush propagates to the
        // inner FlushCounter.
        writer.flush().expect("real flush");
        assert_eq!(
            count.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "guard dropped: a single explicit flush must reach the inner writer",
        );
    }

    /// Even if the closure inside a frame panics, `Drop` on
    /// the guard must clear the flag so the next frame's flush
    /// behaves correctly. Pin that with a panic-and-catch.
    #[test]
    fn defer_flush_guard_clears_flag_on_panic() {
        DEFER_FLUSH.store(false, Ordering::Relaxed);
        let result = std::panic::catch_unwind(|| {
            let _guard = DeferFlushGuard::begin();
            assert!(DEFER_FLUSH.load(Ordering::Relaxed));
            panic!("simulated draw failure");
        });
        assert!(result.is_err(), "panic must propagate out of catch_unwind");
        assert!(
            !DEFER_FLUSH.load(Ordering::Relaxed),
            "guard's Drop must clear DEFER_FLUSH even on panic",
        );
    }

    /// Counts every backend call so the suppressor tests can
    /// assert "the inner backend's `show_cursor` was called
    /// exactly N times." Implements `Backend` minimally — only
    /// the methods exercised by the tests below need real
    /// behavior.
    struct SpyBackend {
        show_cursor_calls: u64,
        hide_cursor_calls: u64,
        set_cursor_calls: u64,
        last_set_to: Option<Position>,
        draw_cells: Vec<(u16, u16)>,
    }

    impl SpyBackend {
        fn new() -> Self {
            Self {
                show_cursor_calls: 0,
                hide_cursor_calls: 0,
                set_cursor_calls: 0,
                last_set_to: None,
                draw_cells: Vec::new(),
            }
        }
    }

    impl io::Write for SpyBackend {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Backend for SpyBackend {
        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            for (x, y, _cell) in content {
                self.draw_cells.push((x, y));
            }
            Ok(())
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.hide_cursor_calls += 1;
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.show_cursor_calls += 1;
            Ok(())
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            Ok(Position::new(0, 0))
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.set_cursor_calls += 1;
            self.last_set_to = Some(position.into());
            Ok(())
        }

        fn clear(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn size(&self) -> io::Result<Size> {
            Ok(Size::new(80, 24))
        }

        fn window_size(&mut self) -> io::Result<WindowSize> {
            Ok(WindowSize {
                columns_rows: Size::new(80, 24),
                pixels: Size::new(0, 0),
            })
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// `show_cursor` must hit the inner backend only on the
    /// first call. Subsequent calls in the same shown-state
    /// window should be no-ops — that's the
    /// `\e[?25h`-per-frame elimination at the heart of the
    /// suppressor.
    #[test]
    fn suppressor_show_cursor_emits_only_on_state_change() {
        let mut s = RedundancySuppressor::new(SpyBackend::new());
        s.show_cursor().unwrap();
        s.show_cursor().unwrap();
        s.show_cursor().unwrap();
        assert_eq!(
            s.inner.show_cursor_calls, 1,
            "show_cursor must forward only on the first call"
        );

        // After hide, the next show again hits the inner.
        s.hide_cursor().unwrap();
        s.show_cursor().unwrap();
        assert_eq!(s.inner.show_cursor_calls, 2, "show after hide must re-emit");
        assert_eq!(s.inner.hide_cursor_calls, 1);
    }

    /// `set_cursor_position` must skip the call when the
    /// target matches the position the cursor is already at —
    /// either from a previous `set_cursor_position` or from
    /// the natural advance after a `draw`.
    #[test]
    fn suppressor_set_cursor_position_drops_redundant_targets() {
        let mut s = RedundancySuppressor::new(SpyBackend::new());

        // First call: inner sees it.
        s.set_cursor_position(Position::new(5, 7)).unwrap();
        assert_eq!(s.inner.set_cursor_calls, 1);

        // Same target: suppressed.
        s.set_cursor_position(Position::new(5, 7)).unwrap();
        s.set_cursor_position(Position::new(5, 7)).unwrap();
        assert_eq!(
            s.inner.set_cursor_calls, 1,
            "matching target must not re-emit"
        );

        // Different target: forwarded.
        s.set_cursor_position(Position::new(8, 7)).unwrap();
        assert_eq!(s.inner.set_cursor_calls, 2);
    }

    /// After `draw` writes a cell at (x, y), the cursor lands
    /// at (x+1, y). A subsequent `set_cursor_position` to that
    /// computed point must be suppressed — this is the
    /// dominant typing-forward case where ratatui repositions
    /// to "where the cell write naturally landed" anyway.
    #[test]
    fn suppressor_set_cursor_after_draw_skips_natural_advance() {
        let mut s = RedundancySuppressor::new(SpyBackend::new());

        // Pretend ratatui's diff wrote one cell at (3, 5).
        // The inspect-tracking inside draw should set
        // last_known_cursor to (4, 5).
        let cell = Cell::default();
        let items = vec![(3_u16, 5_u16, &cell)];
        s.draw(items.into_iter()).unwrap();

        // Now ratatui calls set_cursor_position with the
        // "natural advance" point. Suppressor sees the match
        // and elides the call.
        s.set_cursor_position(Position::new(4, 5)).unwrap();
        assert_eq!(
            s.inner.set_cursor_calls, 0,
            "natural-advance reposition must be suppressed"
        );

        // A reposition to a *different* point should still
        // fire (e.g., user hit an arrow key).
        s.set_cursor_position(Position::new(10, 8)).unwrap();
        assert_eq!(s.inner.set_cursor_calls, 1);
    }

    /// `clear`, `clear_region`, and `append_lines` mutate the
    /// cursor in ways the wrapper can't precisely model, so
    /// they must drop the cached position. The next
    /// `set_cursor_position` after one of these should
    /// unconditionally re-emit (i.e., no false suppression
    /// against a now-stale tracked value).
    #[test]
    fn suppressor_clear_invalidates_cursor_tracking() {
        let mut s = RedundancySuppressor::new(SpyBackend::new());

        s.set_cursor_position(Position::new(5, 7)).unwrap();
        assert_eq!(s.inner.set_cursor_calls, 1);

        s.clear().unwrap();
        // After clear, even an identical-looking position
        // must re-emit.
        s.set_cursor_position(Position::new(5, 7)).unwrap();
        assert_eq!(
            s.inner.set_cursor_calls, 2,
            "set_cursor_position after clear must re-emit"
        );

        s.append_lines(3).unwrap();
        s.set_cursor_position(Position::new(5, 7)).unwrap();
        assert_eq!(
            s.inner.set_cursor_calls, 3,
            "set_cursor_position after append_lines must re-emit"
        );
    }
}
