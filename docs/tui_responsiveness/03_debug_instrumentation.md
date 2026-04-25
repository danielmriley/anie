# PR 3 — Debug instrumentation

**Goal:** Count redraws, log when a frame exceeds a budget, and
make the whole thing gated by an env var so there's zero cost
in normal use. This is the "so we can measure next time"
infrastructure — ship it last, after PRs 1 and 2 have done the
actual work.

## Why last?

Instrumenting *before* the optimization would record what we
already know: "redraws are frequent and expensive." The
instrumentation's real value is verifying that PRs 1 and 2
actually fixed things, plus catching regressions later. Pi
ships similar instrumentation (`PI_DEBUG_REDRAW=1` in
`pi/tui.ts::doRender`); same idea.

## Design

Two signals, both gated on `ANIE_DEBUG_REDRAW=1`:

1. **Redraw counter.** `App` holds an `AtomicU64` of draws
   completed. Exposed via a `render_count()` accessor for tests.
   Always tracked (no env gate), because the cost is one
   atomic increment per frame.

2. **Per-frame log.** When the env var is set, each frame
   appends a single line to `~/.anie/logs/render.log` with:
   - ISO timestamp
   - Draw index
   - Elapsed render time (ms)
   - Number of blocks in the transcript
   - Whether the cache path was hit or missed (future hook;
     stubbed to `-` for now)

   ```
   [2026-04-20T21:14:07.312Z] #1247 4ms blocks=178 cache=-
   ```

## Implementation

One new module, `crates/anie-tui/src/render_debug.rs`, about 30
lines:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

static RENDER_COUNTER: AtomicU64 = AtomicU64::new(0);
static LOG_ENABLED: OnceLock<bool> = OnceLock::new();

fn log_enabled() -> bool {
    *LOG_ENABLED.get_or_init(|| {
        std::env::var("ANIE_DEBUG_REDRAW").ok().as_deref() == Some("1")
    })
}

pub(crate) struct RenderFrame {
    started: Instant,
    index: u64,
}

impl RenderFrame {
    pub(crate) fn begin() -> Self {
        let index = RENDER_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        Self { started: Instant::now(), index }
    }

    pub(crate) fn end(self, block_count: usize) {
        if !log_enabled() {
            return;
        }
        let elapsed_ms = self.started.elapsed().as_millis();
        let Some(log_dir) = anie_config::anie_logs_dir() else { return };
        let _ = std::fs::create_dir_all(&log_dir);
        let path = log_dir.join("render.log");
        let line = format!(
            "[{}] #{} {}ms blocks={}\n",
            chrono::Utc::now().to_rfc3339(),
            self.index,
            elapsed_ms,
            block_count,
        );
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(line.as_bytes())
            });
    }
}

#[cfg(test)]
pub(crate) fn render_count() -> u64 {
    RENDER_COUNTER.load(Ordering::Relaxed)
}
```

Wired into `run_tui`:

```rust
if dirty && last_render_at.elapsed() >= FRAME_BUDGET {
    let frame = render_debug::RenderFrame::begin();
    terminal.draw(|f| app.render(f))?;
    frame.end(app.output_pane().blocks().len());
    // ...
}
```

## Why an atomic, not a field on App?

Test access. `render_count()` can be called from tests that
don't hold a reference to App (they drive terminal/agent events
and then want to assert "how many draws happened?"). A file-level
atomic is simplest.

If multi-App test isolation ever becomes an issue, the atomic can
be replaced with a per-App counter and a test hook; that's a
later change.

## Log rotation

None. The log is opt-in and off by default, and the user will
turn it on for a specific debugging session. If the log grows
too large, the user stops the agent, rotates or deletes the
file, and restarts. Same policy as `anie.log.*`.

## Files

- `crates/anie-tui/src/render_debug.rs` (new, ~50 lines).
- `crates/anie-tui/src/app.rs` — call
  `render_debug::RenderFrame::{begin,end}` around the draw call.
- `crates/anie-tui/src/lib.rs` — `mod render_debug;`.

No changes to `output.rs` (PR 2's work is already in by the
time this lands).

## Test plan

| # | Test |
|---|------|
| 1 | `render_counter_increments_once_per_draw` — drive the App through three events that each dirty the state, advance time past the frame budget between them; assert `render_count()` went up by exactly 3. |
| 2 | `render_counter_stable_when_nothing_changed` — idle App, advance time by 1 s; counter unchanged after the initial draw. |
| 3 | `log_writing_gated_by_env_var` — manually toggle the env var (with `serial_test` or a test-only override function); assert no file is written when disabled, one line per draw when enabled. |

## Exit criteria

- [ ] `render_debug` module lands.
- [ ] Redraw counter increments exactly once per `terminal.draw()`.
- [ ] `ANIE_DEBUG_REDRAW=1` writes one log line per frame.
- [ ] Normal use (env var unset) pays only the atomic increment
      + `OnceLock` load — no syscall, no file IO.
- [ ] Tests 1-3 pass.
- [ ] Manual: run `ANIE_DEBUG_REDRAW=1 cargo run --bin anie`,
      run an agent task, confirm log populates with one line per
      frame.

## What this doesn't do

- **Doesn't render a debug overlay on-screen.** That's a
  different concern (runtime stats in the TUI itself). If we
  want that later, it builds on this counter but is its own PR.
- **Doesn't track cache hit/miss.** The `cache=-` placeholder in
  the log line leaves room. PR 2's cache could expose a hit
  counter as a simple follow-up; explicitly deferred so PR 3
  stays small.
- **Doesn't profile sub-parts of the render.** One wall-clock
  measurement per frame is enough to spot regressions. Finer-
  grained profiling (per-block cost, per-widget cost) is a
  `tracing`-layer decision and doesn't belong in this skill.
