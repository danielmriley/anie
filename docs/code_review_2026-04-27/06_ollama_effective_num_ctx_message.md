# 06 — Use effective Ollama `num_ctx` in load-failure messages

## Rationale

Ollama native requests use the effective context window:

- `crates/anie-cli/src/runtime/config_state.rs:92-95` —
  `effective_ollama_context_window()` returns the active runtime
  override when present, otherwise `model.context_window`.
- `crates/anie-agent/src/agent_loop.rs` snapshots the override into
  `StreamOptions::num_ctx_override` for Ollama native requests.

But the rich user-facing load-failure message receives
`model.context_window`:

- `crates/anie-cli/src/controller.rs:194-210` — controller passes
  `model.context_window` into `render_user_facing_provider_error()`.
- `crates/anie-cli/src/user_error.rs:54-73` — renderer reports that as
  the requested `num_ctx` and computes the halved attempt from it.

If a user runs `/context-length 65536` on a model whose discovered
context is `262144`, and Ollama still fails to load, the message can
incorrectly claim the failed attempts were `262144` and `131072` instead
of `65536` and `32768`.

## Design

Pass the effective value that actually drives the wire request into the
error renderer:

```rust
let requested_num_ctx = self.state.config.effective_ollama_context_window();
render_user_facing_provider_error(
    error,
    requested_num_ctx,
    &model.provider,
    &model.id,
)
```

Keep `render_user_facing_provider_error()` pure and unchanged if
possible. The bug is at the call site: it supplied the wrong value.

## Files to touch

- `crates/anie-cli/src/controller.rs`
  - Change the `ModelLoadResources` rendering call to use
    `effective_ollama_context_window()`.
- `crates/anie-cli/src/user_error.rs`
  - Add or adjust tests if pure renderer tests are enough.
- `crates/anie-cli/src/controller_tests.rs`
  - Add integration-ish controller test with active runtime override if
    the controller harness already supports this.

## Phased PRs

### PR A — Correct the call site and add focused tests

**Change:**

- Replace `model.context_window` with
  `self.state.config.effective_ollama_context_window()` for rich provider
  error rendering.

**Tests:**

- Pure renderer test already covers calculation from input; keep it.
- Add a controller-level regression:
  - current model context window: `262_144`;
  - active override: `65_536`;
  - error: `ModelLoadResources { suggested_num_ctx: 32_768, .. }`;
  - emitted system message contains `num_ctx=65536` and
    `num_ctx=32768`, not `262144` or `131072`.

**Exit criteria:**

- User-facing message names the value actually sent to Ollama.

## Test plan

- `cargo test -p anie-cli user_error`
- `cargo test -p anie-cli controller`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Risks

- The provider's `ModelLoadResources::suggested_num_ctx` on the second
  failed attempt is already based on the halved retry request. Do not
  recompute or overwrite that suggestion in the controller.
- Non-Ollama models should still return `None` from the rich renderer
  unless they use `ModelLoadResources` in the future.

## Exit criteria

- `/context-length` users get accurate load-failure diagnostics.
- Existing no-override message behavior remains unchanged.

## Deferred

- Persisting automatic suggested context changes. This plan only fixes
  messaging; users still opt in via `/context-length`.
