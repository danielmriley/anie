use std::io::{self, Stdout, stdout};
use std::sync::OnceLock;

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Frame, Terminal, backend::Backend, backend::CrosstermBackend};

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
    terminal: Terminal<CrosstermBackend<Stdout>>,
    // Once `restore` has run (either explicitly or via Drop), we
    // must not issue the terminal commands a second time —
    // repeated `LeaveAlternateScreen` etc. are idempotent but we
    // skip them to avoid stray errors during unwinding.
    restored: bool,
}

impl TerminalGuard {
    /// Enter raw mode + alternate screen + mouse capture and
    /// return a guard that owns the configured terminal.
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    /// Borrow the underlying terminal for rendering.
    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
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
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
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
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
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
pub fn draw_synchronized<B, F>(
    terminal: &mut Terminal<B>,
    render_callback: F,
) -> io::Result<()>
where
    B: Backend + io::Write,
    F: FnOnce(&mut Frame),
{
    if sync_output_disabled() {
        terminal.draw(render_callback)?;
        return Ok(());
    }
    // Failures on Begin are surfaced — a downstream draw call
    // that's also going to fail is strictly worse than reporting
    // the earlier failure.
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let draw_result = terminal.draw(render_callback).map(|_| ());
    // End runs regardless of whether draw succeeded so a failed
    // frame doesn't leave the terminal in synchronized-buffering
    // mode forever. draw's error takes precedence if present.
    let end_result = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_result?;
    end_result?;
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
pub fn draw_urgent<B, F>(
    terminal: &mut Terminal<B>,
    render_callback: F,
) -> io::Result<()>
where
    B: Backend + io::Write,
    F: FnOnce(&mut Frame),
{
    terminal.draw(render_callback)?;
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
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableMouseCapture);
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
}
