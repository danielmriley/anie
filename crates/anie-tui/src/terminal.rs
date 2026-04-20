use std::io::{Stdout, stdout};

use anyhow::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

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
