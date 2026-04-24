//! Redraw counter + opt-in per-frame log + JSONL perf spans.
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
//! Opt-in (gated by `ANIE_PERF_TRACE=1`): structured JSONL
//! spans appended to `~/.anie/logs/perf.log.<pid>` for the
//! hot render functions (`build_lines`, `block_lines`,
//! `markdown_render`, `wrap_spans`, `find_link_ranges`,
//! `paragraph_render`). One JSON object per line, easy to
//! parse with `jq` for quick p50/p99 analysis. See
//! `docs/refactor_worklist_2026-04-22/tui_perf_01_baseline_measurement.md`.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static RENDER_COUNTER: AtomicU64 = AtomicU64::new(0);
static LOG_ENABLED: OnceLock<bool> = OnceLock::new();

fn log_enabled() -> bool {
    *LOG_ENABLED.get_or_init(|| std::env::var("ANIE_DEBUG_REDRAW").ok().as_deref() == Some("1"))
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

// --- ANIE_PERF_TRACE=1: structured JSONL span instrumentation.

/// Gate perf instrumentation behind an env var so the 30 fps
/// tick loop doesn't pay the clock-read cost in production.
/// Reads the var once per process; flipping it requires a
/// restart.
pub(crate) fn perf_trace_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("ANIE_PERF_TRACE")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

/// Per-process JSONL log file handle. Opened on first span
/// emission; subsequent spans reuse the handle via the Mutex.
/// `None` means the open failed (no log dir, read-only FS, etc.)
/// and the perf system degrades silently.
static PERF_LOG: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn perf_log() -> Option<&'static Mutex<std::fs::File>> {
    PERF_LOG
        .get_or_init(|| {
            let dir = anie_config::anie_logs_dir()?;
            std::fs::create_dir_all(&dir).ok()?;
            let path = dir.join(format!("perf.log.{}", std::process::id()));
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .ok()
                .map(Mutex::new)
        })
        .as_ref()
}

/// Span kinds recorded by the perf-trace system. Short,
/// stable string labels — tests and `jq` filters depend on
/// these exact values.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PerfSpanKind {
    BuildLines,
    BlockLines,
    MarkdownRender,
    WrapSpans,
    FindLinkRanges,
    ParagraphRender,
}

impl PerfSpanKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::BuildLines => "build_lines",
            Self::BlockLines => "block_lines",
            Self::MarkdownRender => "markdown_render",
            Self::WrapSpans => "wrap_spans",
            Self::FindLinkRanges => "find_link_ranges",
            Self::ParagraphRender => "paragraph_render",
        }
    }
}

/// Scope-based instrumentation span. Create with
/// [`PerfSpan::enter`] at the top of the instrumented function;
/// attach fields with [`PerfSpan::record`]; the span flushes to
/// the JSONL log on drop. Returns `None` when
/// `ANIE_PERF_TRACE` is unset so entering a span in production
/// costs one `OnceLock` load.
pub(crate) struct PerfSpan {
    kind: PerfSpanKind,
    started: Instant,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl PerfSpan {
    /// Begin a span. Returns `None` when perf tracing is off.
    #[must_use]
    pub(crate) fn enter(kind: PerfSpanKind) -> Option<Self> {
        if !perf_trace_enabled() {
            return None;
        }
        Some(Self {
            kind,
            started: Instant::now(),
            fields: serde_json::Map::new(),
        })
    }

    /// Attach a field to the span. Accepts anything convertible
    /// to `serde_json::Value` (u64, i64, &str, String, bool).
    pub(crate) fn record(&mut self, key: &str, value: impl Into<serde_json::Value>) {
        self.fields.insert(key.to_string(), value.into());
    }

    /// Produce the JSON line that would be written for this span.
    /// Exposed separately from `Drop` so tests can assert on the
    /// output without touching the global log file.
    ///
    /// Field precedence: reserved keys (`kind`, `elapsed_us`,
    /// `ts_ms`) are authoritative. If a caller recorded a
    /// same-named custom field, the reserved value wins. This
    /// protects jq aggregations that group on `kind`.
    fn serialize_line(&mut self, elapsed_us: u64, ts_ms: u64) -> String {
        let mut obj = serde_json::Map::new();
        for (k, v) in std::mem::take(&mut self.fields) {
            obj.insert(k, v);
        }
        // Reserved fields inserted AFTER custom fields so they
        // overwrite any collision.
        obj.insert(
            "kind".into(),
            serde_json::Value::String(self.kind.as_str().into()),
        );
        obj.insert("elapsed_us".into(), serde_json::Value::from(elapsed_us));
        obj.insert("ts_ms".into(), serde_json::Value::from(ts_ms));
        let mut line = serde_json::to_string(&obj).unwrap_or_default();
        line.push('\n');
        line
    }
}

impl Drop for PerfSpan {
    fn drop(&mut self) {
        let Some(lock) = perf_log() else { return };
        let elapsed_us = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let ts_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        )
        .unwrap_or(0);
        let line = self.serialize_line(elapsed_us, ts_ms);
        if let Ok(mut file) = lock.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }
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

    #[test]
    fn perf_span_kind_as_str_matches_documented_labels() {
        // `jq` filters and this test pin the public contract for
        // the `kind` field in the JSONL output.
        assert_eq!(PerfSpanKind::BuildLines.as_str(), "build_lines");
        assert_eq!(PerfSpanKind::BlockLines.as_str(), "block_lines");
        assert_eq!(PerfSpanKind::MarkdownRender.as_str(), "markdown_render");
        assert_eq!(PerfSpanKind::WrapSpans.as_str(), "wrap_spans");
        assert_eq!(PerfSpanKind::FindLinkRanges.as_str(), "find_link_ranges");
        assert_eq!(PerfSpanKind::ParagraphRender.as_str(), "paragraph_render");
    }

    #[test]
    fn serialize_line_emits_valid_jsonl_with_reserved_and_custom_fields() {
        let mut span = PerfSpan {
            kind: PerfSpanKind::BuildLines,
            started: Instant::now(),
            fields: serde_json::Map::new(),
        };
        span.record("blocks", 42_u64);
        span.record("slowest_miss_block", "assistant");
        let line = span.serialize_line(1234, 5678);
        assert!(line.ends_with('\n'));
        let trimmed = line.trim_end();
        let parsed: serde_json::Value =
            serde_json::from_str(trimmed).expect("output must parse as valid JSON");
        assert_eq!(parsed["kind"], "build_lines");
        assert_eq!(parsed["elapsed_us"], 1234);
        assert_eq!(parsed["ts_ms"], 5678);
        assert_eq!(parsed["blocks"], 42);
        assert_eq!(parsed["slowest_miss_block"], "assistant");
    }

    #[test]
    fn serialize_line_empty_fields_yields_only_reserved_keys() {
        let mut span = PerfSpan {
            kind: PerfSpanKind::ParagraphRender,
            started: Instant::now(),
            fields: serde_json::Map::new(),
        };
        let line = span.serialize_line(10, 20);
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        let obj = parsed.as_object().expect("object");
        assert_eq!(obj.len(), 3, "only kind, elapsed_us, ts_ms expected");
        assert!(obj.contains_key("kind"));
        assert!(obj.contains_key("elapsed_us"));
        assert!(obj.contains_key("ts_ms"));
    }

    #[test]
    fn serialize_line_reserved_kind_wins_over_custom_collision() {
        // Documented contract: reserved keys (kind, elapsed_us,
        // ts_ms) are authoritative. A caller who accidentally
        // uses one of these names for a custom field cannot break
        // jq aggregations that group on `kind`.
        let mut span = PerfSpan {
            kind: PerfSpanKind::BuildLines,
            started: Instant::now(),
            fields: serde_json::Map::new(),
        };
        span.record("kind", "custom_override");
        span.record("elapsed_us", 999_999_u64);
        span.record("ts_ms", 0_u64);
        let line = span.serialize_line(1, 2);
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).expect("valid JSON");
        assert_eq!(parsed["kind"], "build_lines");
        assert_eq!(parsed["elapsed_us"], 1);
        assert_eq!(parsed["ts_ms"], 2);
    }
}
