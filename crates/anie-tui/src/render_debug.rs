//! Redraw counter + opt-in per-frame log.
//!
//! Always-on: one atomic increment per completed frame, exposed
//! through `render_count()` for tests that want to assert on
//! redraw cadence.
//!
//! Opt-in (gated by `ANIE_DEBUG_REDRAW=1`): one line per frame
//! appended to `~/.anie/logs/render.log` with the frame index,
//! elapsed ms, and current block count. Useful for measuring
//! whether a change regressed the render profile — see
//! `docs/tui_responsiveness/03_debug_instrumentation.md`.
//!
//! Mirrors pi's `PI_DEBUG_REDRAW=1` channel in
//! `pi/tui.ts::doRender`. Like pi, normal use pays only the
//! atomic increment; no syscall, no file IO.
//!
//! Log rotation is deliberately absent — the log is opt-in, user
//! enables it for a specific debugging session, stops the agent,
//! and inspects or rotates the file manually. Matches the
//! `anie.log.*` rolling-append policy.

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static RENDER_COUNTER: AtomicU64 = AtomicU64::new(0);
static LOG_ENABLED: OnceLock<bool> = OnceLock::new();

fn log_enabled() -> bool {
    *LOG_ENABLED.get_or_init(|| {
        std::env::var("ANIE_DEBUG_REDRAW").ok().as_deref() == Some("1")
    })
}

/// Scope guard for one paint cycle. Create at the top of the
/// draw with `RenderFrame::begin()` and call `end(block_count)`
/// after `terminal.draw(...)` returns. The render counter
/// increments in `begin`; the optional log line is written in
/// `end`.
pub(crate) struct RenderFrame {
    started: Instant,
    index: u64,
}

impl RenderFrame {
    pub(crate) fn begin() -> Self {
        // `fetch_add` returns the pre-increment value; `+1`
        // turns it into a human-friendly 1-based frame index.
        let index = RENDER_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        Self {
            started: Instant::now(),
            index,
        }
    }

    pub(crate) fn end(self, block_count: usize) {
        if !log_enabled() {
            return;
        }
        let elapsed_ms = self.started.elapsed().as_millis();
        let Some(log_dir) = anie_config::anie_logs_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&log_dir);
        let path = log_dir.join("render.log");
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let line = format!(
            "[{timestamp_ms}] #{} {elapsed_ms}ms blocks={block_count}\n",
            self.index,
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// Current total frames rendered since process start. Exposed
/// for tests that assert redraw counts; production code does not
/// read this.
#[cfg(test)]
pub(crate) fn render_count() -> u64 {
    RENDER_COUNTER.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn begin_increments_counter_exactly_once() {
        let before = render_count();
        {
            let _frame = RenderFrame::begin();
            let after_begin = render_count();
            assert_eq!(after_begin, before + 1);
            // Leaving the scope drops _frame; it has no Drop
            // impl, so dropping doesn't double-count.
        }
        assert_eq!(render_count(), before + 1);
    }

    #[test]
    fn end_without_log_enabled_is_noop() {
        // We can't mutate LOG_ENABLED after it's set, so we rely
        // on the default (env var unset in test runner). Just
        // verify `end()` doesn't panic and doesn't throw.
        let frame = RenderFrame::begin();
        frame.end(42);
    }
}
