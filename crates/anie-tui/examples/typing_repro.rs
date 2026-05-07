//! Standalone typing-latency reproducer for round-9 testing of
//! `docs/code_review_2026-05-03.md`.
//!
//! Emits exactly the byte pattern anie produces per keystroke
//! (cursor move + char + SGR resets ≈ 22 bytes), but with NO
//! ratatui, NO async runtime, NO event-loop dispatch chain.
//! Just: read key, write bytes, flush. One blocking thread.
//!
//! Purpose: separate two hypotheses for the residual typing
//! drag the user reports:
//! 1. The byte pattern itself is what VTE / Kitty render
//!    slowly — feel here will match anie.
//! 2. anie's pipeline (tokio + crossterm EventStream + render
//!    closure + ratatui diff) adds latency the per-frame
//!    `t_key_to_paint_us` trace can't see — feel here will
//!    be noticeably better than anie.
//!
//! Run from the workspace root:
//!
//! ```bash
//! cargo run --example typing_repro -p anie-tui --release
//! ```
//!
//! Then type a sentence the way you would in anie. Compare
//! the perceived latency to anie typing in the same terminal.
//! Press Ctrl-C to exit.

use std::io::{self, Write, stdout};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, read};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

fn main() -> io::Result<()> {
    println!("anie typing-pattern reproducer");
    println!("Each keystroke emits the exact 22-byte pattern anie sends:");
    println!("    \\e[<row>;<col>H<char>\\e[39m\\e[49m\\e[0m");
    println!("Type a sentence. Press Ctrl-C to exit.");
    println!();

    enable_raw_mode()?;
    let outcome = run();
    disable_raw_mode()?;
    println!();
    outcome
}

fn run() -> io::Result<()> {
    let mut stdout = stdout();
    // Pretend we're inside an input box: start at row 5, col 1.
    // The exact row/col doesn't matter; what matters is that the
    // emitted byte length matches anie's (cursor coords are 1–2
    // digits each, same as anie's typical input area).
    let row: u16 = 5;
    let mut col: u16 = 1;

    loop {
        // Blocking read — no async, no event loop, no
        // intermediate dispatch. Closest thing to "kernel hands
        // us a key, we write a char."
        let event = read()?;
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        else {
            continue;
        };

        // Ctrl-C: clean exit.
        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return Ok(());
        }

        // Backspace: emit the inverse of a typed char so the
        // tester can correct typos naturally without confusing
        // the reproducer.
        if code == KeyCode::Backspace && col > 1 {
            col -= 1;
            // Move cursor back, write space, move cursor back
            // again — same pattern most editors use, but this
            // reproducer doesn't care about realism, only about
            // the typed-char hot path. So this branch is just
            // ergonomic; not part of the latency test.
            write!(stdout, "\x1b[{row};{col}H \x1b[{row};{col}H")?;
            stdout.flush()?;
            continue;
        }

        // Only printable chars: typed-char hot path.
        let KeyCode::Char(ch) = code else {
            continue;
        };
        // Reject Ctrl-modified chars (Ctrl-A, Ctrl-K, etc.) so
        // they don't pollute the test.
        if modifiers.contains(KeyModifiers::CONTROL) || modifiers.contains(KeyModifiers::ALT) {
            continue;
        }

        // anie's exact byte pattern for one typed character:
        //   \e[row;colH<char>\e[39m\e[49m\e[0m
        // ratatui emits the cursor move via crossterm's MoveTo,
        // the char via Print, and the trailing SGR resets via
        // SetForegroundColor(Reset) + SetBackgroundColor(Reset)
        // + SetAttribute(Reset). With anie's RedundancySuppressor
        // in place, the post-draw show_cursor + cursor reposition
        // are suppressed, so what hits the wire is just this:
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        write!(stdout, "\x1b[{row};{col}H{s}\x1b[39m\x1b[49m\x1b[0m")?;
        // Single explicit flush — same as anie's
        // DeferredFlushBufWriter behavior of one syscall per
        // frame. Without this, stdout's internal LineWriter
        // would hold bytes until a newline.
        stdout.flush()?;

        col = col.saturating_add(1);
    }
}
