# TUI responsiveness — execution

Tracker for the three PRs. Update status inline as each lands.

## Baseline

Partial fix in `df2d2c4` (drain agent events + dirty flag) is
already on `feat/provider-compat-blob`. These PRs stack on top.

| PR | Status | Commit |
|----|--------|--------|
| [PR 1 — render scheduling + 30 fps cap](../01_render_scheduling.md) | pending | — |
| [PR 2 — per-block line cache](../02_output_pane_cache.md) | pending (blocked by PR 1) | — |
| [PR 3 — debug instrumentation](../03_debug_instrumentation.md) | pending (blocked by PR 2) | — |

## Ordering

PR 1 first: cheapest change, obvious correctness, bounds the
rate cost. Validates end-to-end before we touch the hotter
hot-path in PR 2.

PR 2 second: structural fix for per-frame cost. Most of the work
and most of the user-visible win.

PR 3 third: instrumentation against the already-fixed baseline
so the first real log lines show the *new* redraw profile, not
the old one.

## Gate per PR

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Plus a manual smoke: long agent run (e.g. the Nemotron session
that surfaced the original bug), keystrokes during streaming,
scroll through history. No input lag.
