# Plan 08 — Small hygiene items

A bundle of individually small items called out in the review. Each
is its own phase so they can be picked up one at a time without
coordination. Each phase touches ≤3 files.

> **Status (2026-04-17):**
> - **Phase A (`.anie/` paths):** Complete on `refactor_branch`.
>   Added `anie_dir`, `anie_auth_json_path`, `anie_sessions_dir`,
>   `anie_logs_dir`, `anie_state_json_path` helpers to
>   `anie-config`; all callers updated.
> - **Phase B (HTTP panics):** Not landed — plan 04 phase 1
>   provides the better fix (shared `http::client() ->
>   Result<...>`). The current `#[allow(clippy::expect_used)]`
>   with justification is the interim.
> - **Phase C (`.expect` audit):** The production-code sites
>   flagged in the review (onboarding.rs:1682, model_picker
>   tests) were addressed during the plan 00 CI followup —
>   test-module uses are now covered by the workspace
>   `cfg_attr(test, allow(clippy::expect_used))`.
> - **Phase D (event send logging):** Not landed. Queued.
> - **Phase E (cached ToolRegistry):** Was already done before
>   this plan was written — `ControllerState` caches it once at
>   `prepare_controller_state` time and clones the `Arc` per
>   run.
> - **Phase F (borrowing context API):** Complete on
>   `refactor_branch`. Added
>   `SessionManager::estimate_context_tokens(&self)` which walks the
>   active branch without cloning messages; migrated
>   `SessionHandle::estimated_context_tokens`, `auto_compact`, and
>   `force_compact` to use it. Parity test locks the counts to the
>   old `build_context()` path.

## Motivation

These didn't fit cleanly into plans 01–07:

- `.anie/` path construction is duplicated across `anie-config` and
  `anie-auth`.
- `.expect()` / `.unwrap()` on `reqwest::Client::builder()` in
  `http.rs:10` and `local.rs:91–94`.
- `.expect("selected model")` in `model_picker.rs:542, 562`.
- `let _ = event_tx.send(...)` swallowing channel send failures in
  `agent_loop.rs:343-348` and several controller sites.
- `session.build_context()` called multiple times per turn with full
  message-vector clones (`controller.rs:609, 635-639, 987-1003`).
- `ToolRegistry` rebuilt per agent run (`controller.rs:969-984`) —
  partly addressed by plan 03 phase 5, but standalone-fixable too.

## Design principles

1. **One PR per phase.** Each phase is reviewable in fifteen
   minutes.
2. **Behavior preserving.** None of these change user-visible
   behavior.
3. **Clippy stays green.** Each phase preserves the workspace's
   lint gate.

---

## Phase A — Consolidate `.anie/` path construction

**Goal:** One function returns `~/.anie/`. Everyone uses it.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-config/src/lib.rs` | Add `pub fn anie_dir() -> Option<PathBuf>` that returns `dirs::home_dir()?.join(".anie")`; migrate internal callers |
| `crates/anie-auth/src/lib.rs` / `store.rs` | Use `anie_config::anie_dir()` instead of local `home_dir` + `.anie` joins |

### Sub-step A — Grep the duplication

```
grep -rn '".anie"' crates/ | grep -v test
```

Expect hits in `anie-config` (config path), `anie-auth` (auth.json
fallback, sessions dir? — no, sessions is in session crate but the
path construction might still leak).

### Sub-step B — Single helper

```rust
/// The anie user directory, typically `~/.anie/`. Returns `None` if
/// the home directory cannot be determined.
pub fn anie_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".anie"))
}

pub fn anie_config_path() -> Option<PathBuf> {
    anie_dir().map(|d| d.join("config.toml"))
}

pub fn anie_auth_json_path() -> Option<PathBuf> {
    anie_dir().map(|d| d.join("auth.json"))
}
```

### Sub-step C — Migrate callers

Each file that constructs `.anie/...` directly now goes through a
helper. No file outside `anie-config` should have the literal
`".anie"`.

### Test plan

| # | Test |
|---|------|
| 1 | `anie_dir_respects_home_dir` (env-munged test via `temp-env`) |
| 2 | `anie_config_path_is_dir_plus_config_toml` |
| 3 | Existing auth / config tests pass unchanged. |

### Exit criteria

- [ ] `grep -rn '".anie"' crates/` returns only `anie-config/src/
      lib.rs` definition sites + tests.
- [ ] All consumers go through the helper.

---

## Phase B — Stop panicking on HTTP client creation

**Goal:** `reqwest::Client` builder failures surface as errors, not
panics.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/http.rs` | Replace `.expect(...)` with proper error propagation |
| `crates/anie-providers-builtin/src/local.rs` | Same (lines 91–94) |

### Sub-step A — `http.rs`

Current:

```rust
static CLIENT: OnceLock<Client> = OnceLock::new();

pub fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        Client::builder()
            .build()
            .expect("build reqwest client")
    })
}
```

Target:

```rust
static CLIENT: OnceLock<Result<Client, Arc<reqwest::Error>>> = OnceLock::new();

pub fn client() -> Result<&'static Client, ProviderError> {
    match CLIENT.get_or_init(|| {
        Client::builder().build().map_err(Arc::new)
    }) {
        Ok(c) => Ok(c),
        Err(e) => Err(ProviderError::Transport(e.to_string())),
    }
}
```

(Or whichever error variant plan 05 defines. If plan 05 hasn't
landed, use `ProviderError::Other` temporarily and flag for
cleanup.)

Callers that currently do `http::client().get(...)` gain a `?` —
this is the only behavior change.

### Sub-step B — `local.rs`

Same pattern. `local.rs` builds a client for local-server detection;
if TLS roots fail, the feature should report "no server detected,"
not panic.

### Test plan

| # | Test |
|---|------|
| 1 | `client_returns_ok_under_normal_tls_roots` |
| 2 | Existing integration tests pass. |
| 3 | Manual: simulate a TLS-roots failure (tricky to do automatically; document as "best-effort, verified by reading the code path"). |

### Exit criteria

- [ ] No `.expect` on `Client::builder().build()` in the crate.
- [ ] Callers propagate via `?`.

---

## Phase C — Audit `.expect()` / `.unwrap()` in hot paths

**Goal:** Remove remaining production-code `.expect` / `.unwrap` calls
flagged by the review.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-tui/src/model_picker.rs` | Replace `.expect("backend size")` and `.expect("selected model")` with proper `Option` handling |
| `crates/anie-tui/src/onboarding.rs` | Review `.expect("providers should not be empty")` site at line 1687 — if the invariant is real, add a comment; if it can be violated via a race, handle it |

### Sub-step A — `model_picker.rs`

The `selected_model()` method currently:

```rust
self.models.get(self.selected).expect("selected model")
```

Replace with:

```rust
self.models.get(self.selected)
```

and have callers handle `None` by ignoring the keypress (for arrow
keys) or showing an error state.

The index can lag the backing vec on refresh — that's a real race,
not an invariant. Treating it as an invariant is why this bug stays
invisible.

### Sub-step B — `onboarding.rs:1687`

Read the surrounding code. If the assertion is genuinely
unconditional (e.g., the code path is only reachable when a check
has been done), keep it but add a comment:

```rust
// Safety: we only enter this path after `providers` was validated non-empty
// at line NNN. If that changes, this expect will fail loudly, which is
// preferable to silent no-op.
```

If it can fail (e.g., worker events race with UI), handle the empty
case.

### Test plan

| # | Test |
|---|------|
| 1 | `model_picker_with_empty_list_does_not_panic_on_arrow_keys` |
| 2 | `model_picker_index_reset_on_list_shrink` |
| 3 | Clippy still clean. |

### Exit criteria

- [ ] Zero `.expect` / `.unwrap` on fallible `Option`/`Result` values
      in `model_picker.rs`.
- [ ] Any remaining `.expect` has a comment justifying it.

---

## Phase D — Log on channel send failure

**Goal:** `let _ = event_tx.send(...)` stops swallowing first-time
failures silently.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-agent/src/agent_loop.rs` | Add a helper for "send event, warn-once on failure"; migrate the `let _ = event_tx.send(...)` sites |
| `crates/anie-cli/src/controller.rs` | Same migration in controller sites |

### Sub-step A — Helper

```rust
fn send_or_warn<T: std::fmt::Debug>(
    tx: &mpsc::Sender<T>,
    item: T,
    site: &'static str,
    warned: &AtomicBool,
) {
    if tx.try_send(item).is_err() && !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(site, "event channel closed; subsequent events will be dropped silently");
    }
}
```

Or, if `send` is async (it is for `tokio::sync::mpsc::Sender`):

```rust
async fn send_or_warn<T: std::fmt::Debug>(
    tx: &mpsc::Sender<T>,
    item: T,
    site: &'static str,
    warned: &AtomicBool,
) {
    if tx.send(item).await.is_err() && !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(site, "event channel closed; subsequent events will be dropped silently");
    }
}
```

The `AtomicBool` ensures we warn once per run, not on every drop.

### Sub-step B — Migrate

Grep `let _ = .*event_tx.send` and replace each with
`send_or_warn(&event_tx, event, "site-name", &warned).await`.

### Test plan

| # | Test |
|---|------|
| 1 | `send_or_warn_logs_once_when_channel_closed` (using a `tracing-test` subscriber) |
| 2 | `send_or_warn_does_not_log_on_first_success` |
| 3 | `existing_agent_loop_tests_unaffected` |

### Exit criteria

- [ ] Zero `let _ = event_tx.send(...)` in `agent_loop.rs` and
      `controller.rs`.
- [ ] Channel closure produces exactly one warn log per run.

---

## Phase E — Cache `ToolRegistry` on `ControllerState`

**Goal:** Build once; reuse.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-cli/src/controller.rs` | Move `build_tool_registry(...)` call to `ControllerState::new`; store `Arc<ToolRegistry>`; hand `Arc::clone(...)` into `AgentLoopConfig` |

### Sub-step A — Move the build

Currently at lines 969–984, `build_tool_registry(cwd, no_tools)` is
called inside the per-run setup. Move it to
`prepare_controller_state` / `ControllerState::new` so it runs once
at startup.

### Sub-step B — Handle `--cwd` changes

If the CLI supports changing cwd mid-session (does it?), the tool
registry's behavior shouldn't depend on cwd — `BashTool` and
file tools resolve paths at execution time, not at registry build.
Confirm by reading `anie-tools/src/bash.rs` and
`anie-tools/src/shared.rs`. If there's a cwd capture in the
registry, defer this phase until the registry is cwd-independent.

### Test plan

| # | Test |
|---|------|
| 1 | `controller_state_holds_tool_registry` |
| 2 | Existing tool-call integration tests pass. |
| 3 | `bash_tool_respects_runtime_cwd_not_registry_cwd` (only if a cwd concern exists) |

### Exit criteria

- [ ] `build_tool_registry` is called once per `ControllerState`.
- [ ] Per-run code clones an `Arc`, not builds fresh.

---

## Phase F — Cheap context-building improvements

**Goal:** Reduce redundant `session.build_context()` calls and
full-vector clones per turn.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | Add `fn latest_leaf_messages(&self, filter: impl Fn(&Entry) -> bool) -> Vec<&Message>` or similar borrow-returning API |
| `crates/anie-cli/src/controller.rs` | Migrate the three `session.build_context()` call sites (`controller.rs:609, 635-639, 987-1003`) where a borrow suffices |

### Sub-step A — Identify ownership needs

Of the three call sites, which truly need an owned `Vec<Message>`?

- The one fed to `AgentLoop::run` needs ownership because it's
  passed across an `async` boundary.
- The one used for a local filter + count — doesn't.
- The one used for continuation runs — probably does if the
  continuation is async.

Only migrate where a borrow works.

### Sub-step B — New session API

```rust
pub fn iter_context(&self) -> impl Iterator<Item = &Entry> {
    // walk from leaf_id up through parent_ids, yielding message
    // entries only, in turn order.
}
```

Callers that only need to count, filter, or scan can use this
without allocating.

### Test plan

| # | Test |
|---|------|
| 1 | `iter_context_yields_in_turn_order` |
| 2 | `iter_context_matches_build_context_under_identity_mapping` |
| 3 | Performance: informal — a session with 1000 entries should not measurably slow down per-turn setup. Don't over-engineer a benchmark. |

### Exit criteria

- [ ] Non-owning callers use the iterator.
- [ ] Owning callers remain on `build_context()`.
- [ ] No per-turn work that used to be O(1) became O(n).

---

## Phase ordering

All six phases are independent. Pick any one in any order. Each is
individually reviewable.

## Out of scope

- Anything in plans 00–07. If a hygiene item fits better there, it
  belongs there.
- Performance benchmarking infrastructure (tracked in
  `docs/ideas.md`).
- Tracing-attribute enrichment across the workspace (bigger
  observability story).
