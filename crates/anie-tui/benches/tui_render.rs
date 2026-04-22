// Benchmark harness: panics are acceptable — a failing bench
// should surface loudly, not degrade silently.
#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Criterion benchmark for the `OutputPane` render pipeline.
//!
//! Three scenarios, all driven against a `TestBackend` so the
//! benchmark is headless and repeatable:
//!
//! 1. **`scroll_static_600`** — 600 finalized mixed-markdown
//!    blocks. Caches warm. Measures the cache-hit render cost,
//!    which is the median-case steady-state price for a long
//!    transcript with no new content arriving.
//! 2. **`stream_into_static_600`** — 600 finalized blocks plus
//!    one streaming assistant block receiving 5-char deltas.
//!    The streaming block bypasses the line cache, so each
//!    iteration exercises the delta append + re-wrap path.
//! 3. **`resize_during_stream`** — 600 finalized blocks with an
//!    alternating-width render. Forces cache invalidation on
//!    roughly half the iterations, simulating drag-resize.
//!
//! See `docs/refactor_worklist_2026-04-22/tui_perf_01_baseline_measurement.md`
//! for the before/after protocol.

use std::hint::black_box;

use anie_tui::OutputPane;
use criterion::{Criterion, criterion_group, criterion_main};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const VIEWPORT_WIDTH: u16 = 120;
const VIEWPORT_HEIGHT: u16 = 40;

/// Build a pane containing `turns` finalized assistant turns,
/// each with a mixed-markdown response body (heading, prose,
/// bullets, fenced Rust code block, blockquote, link). Mirrors
/// the `build_markdown_transcript` helper in the in-crate
/// stress test but is replicated here since that helper is
/// `#[cfg(test)]` only.
fn build_markdown_transcript(turns: usize) -> OutputPane {
    let mut pane = OutputPane::new();
    for turn in 0..turns {
        let turn_u64 = turn as u64;
        pane.add_user_message(format!("Question {turn} about something"), turn_u64);
        pane.add_streaming_assistant();
        let body = format!(
            "## Answer {turn}\n\nHere's a longer prose paragraph. It has \
             **bold** text, *italic*, and some `inline code` spanning \
             multiple words. Also a link: [the docs](https://example.com/docs/{turn}).\n\n\
             - bullet one\n\
             - bullet two with `code`\n\
             - bullet three\n\n\
             ```rust\n\
             fn main() {{\n    println!(\"turn {turn}\");\n}}\n\
             ```\n\n\
             > a blockquote reminder\n\n\
             Final paragraph tying it together.",
        );
        pane.append_to_last_assistant(&body);
        pane.finalize_last_assistant(body, String::new(), turn_u64, None);
    }
    pane
}

fn new_terminal(width: u16, height: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(width, height)).expect("terminal")
}

fn render_once(pane: &mut OutputPane, terminal: &mut Terminal<TestBackend>) {
    terminal
        .draw(|frame| {
            let area = frame.area();
            pane.render(area, frame.buffer_mut(), ".");
        })
        .expect("draw");
}

fn bench_scroll_static_600(c: &mut Criterion) {
    let mut pane = build_markdown_transcript(600);
    let mut terminal = new_terminal(VIEWPORT_WIDTH, VIEWPORT_HEIGHT);
    // Warm caches. First render parses markdown + fills
    // `LineCache` for every block.
    render_once(&mut pane, &mut terminal);

    c.bench_function("scroll_static_600", |b| {
        b.iter(|| {
            render_once(black_box(&mut pane), black_box(&mut terminal));
        });
    });
}

fn bench_stream_into_static_600(c: &mut Criterion) {
    // 600 finalized blocks + one active streaming assistant.
    let mut pane = build_markdown_transcript(600);
    pane.add_streaming_assistant();
    let mut terminal = new_terminal(VIEWPORT_WIDTH, VIEWPORT_HEIGHT);
    render_once(&mut pane, &mut terminal);

    c.bench_function("stream_into_static_600", |b| {
        b.iter(|| {
            // 5-char delta per iteration, simulating a typical
            // token chunk size. The streaming block skips the
            // cache, so every iteration re-wraps whatever has
            // accumulated so far.
            pane.append_to_last_assistant("hello");
            render_once(black_box(&mut pane), black_box(&mut terminal));
        });
    });
}

fn bench_resize_during_stream(c: &mut Criterion) {
    let mut pane = build_markdown_transcript(600);
    let mut terminal_wide = new_terminal(VIEWPORT_WIDTH, VIEWPORT_HEIGHT);
    let mut terminal_narrow = new_terminal(VIEWPORT_WIDTH - 20, VIEWPORT_HEIGHT);
    render_once(&mut pane, &mut terminal_wide);
    render_once(&mut pane, &mut terminal_narrow);

    let mut iter = 0u64;
    c.bench_function("resize_during_stream", |b| {
        b.iter(|| {
            // Alternate widths every iteration so roughly half
            // the renders hit cache misses for width changes.
            // Real drag-resize is burstier than this but this
            // gives a stable upper-bound on the cost.
            if iter % 2 == 0 {
                render_once(black_box(&mut pane), black_box(&mut terminal_wide));
            } else {
                render_once(black_box(&mut pane), black_box(&mut terminal_narrow));
            }
            iter += 1;
        });
    });
}

criterion_group!(
    tui_render,
    bench_scroll_static_600,
    bench_stream_into_static_600,
    bench_resize_during_stream,
);
criterion_main!(tui_render);
