# 04 — Web cancellation, configurable budgets, and bounded side channels

## Rationale

The review found two related problems in the new web tools:

1. `web_read` / `web_search` ignore the cancellation token passed by the
   agent loop:
   - `crates/anie-tools-web/src/read/tool.rs:167-180`
   - `crates/anie-tools-web/src/search/tool.rs:123-136`
   - `crates/anie-agent/src/agent_loop.rs:892-900`
2. Some adjacent reads are unbounded even though normal page bodies are
   capped:
   - `crates/anie-tools-web/src/read/fetch.rs:334-341` — non-2xx error
     body uses `response.text().await` before truncating.
   - `crates/anie-tools-web/src/read/fetch.rs:214-224` — `robots.txt`
     uses `response.bytes().await` without a cap.
   - `crates/anie-tools-web/src/read/extract.rs:160-169` — Defuddle
     uses `wait_with_output()`, collecting all stdout/stderr.

The user explicitly wants anie to grow into a long-running persistent
agent. This plan therefore treats cancellation and memory caps as
correctness requirements, while treating wall-clock timeouts as
centralized, configurable policy.

## Design

### Non-negotiable behavior

- User/controller cancellation must interrupt web tools promptly.
- Child processes spawned by web tools must be killed on cancellation.
- Error/side-channel bodies must have memory caps.
- Bounded reads should return typed `WebToolError`s with useful excerpts.

### Timeout/budget philosophy

Avoid hidden short hard stops. Instead:

- Keep the existing fetch timeout as a configurable default, not a new
  per-call magic number.
- Add budget fields under one config surface, likely `[tools.web]`, so
  long-running deployments can relax or disable wall-clock budgets.
- Prefer **idle/no-progress** budgets for subprocesses where possible.
- Make `0` or an explicit `"none"` value mean "no wall-clock cap" only
  if validation and docs are clear. Do not silently treat invalid values
  as disabled.

### Proposed config shape

Keep the first config surface small. Example target:

```toml
[tools.web]
# Existing behavior made explicit; generous enough for ordinary pages.
request_timeout_secs = 30
headless_timeout_secs = 30

# Defuddle is an external runtime dependency and can pay npx/npm cost on
# first use. Keep this generous, and allow operators to disable it for
# persistent-agent deployments if needed.
defuddle_timeout_secs = 300

# Memory caps stay hard safety limits.
max_page_bytes = 10485760
max_error_body_bytes = 262144
max_robots_bytes = 524288
max_subprocess_output_bytes = 1048576
```

The exact field names can be refined in PR C. The important constraint:
time/budget defaults are centralized and documented, not hardcoded at
call sites.

## Files to touch

- `crates/anie-tools-web/src/read/tool.rs`
  - Thread `CancellationToken` into `run()` and its sub-steps.
- `crates/anie-tools-web/src/search/tool.rs`
  - Thread cancellation into `run()` and backend fetch.
- `crates/anie-tools-web/src/read/fetch.rs`
  - Add bounded body helper(s) shared by success/error/robots paths.
  - Make timeout/lifetime settings come from a central options struct.
- `crates/anie-tools-web/src/read/extract.rs`
  - Replace `wait_with_output()` with bounded stdout/stderr collection.
  - Kill the child on cancellation or configured timeout.
- `crates/anie-tools-web/src/error.rs`
  - Add typed variants if needed (`SubprocessTimeout`,
    `SideChannelTooLarge`, etc.).
- `crates/anie-config/src/lib.rs`
  - Add optional `[tools.web]` config in PR C.
- `crates/anie-cli/src/bootstrap.rs`
  - Pass web config into `anie_tools_web::web_tools(...)` or a builder.

## Phased PRs

### PR A — Cancellation without new timeout policy

**Change:**

- Change `WebReadTool::execute` and `WebSearchTool::execute` to pass the
  cancellation token into internal `run()` methods.
- Wrap rate-limiter waits, HTTP fetch futures, headless render futures,
  and Defuddle runner futures in `tokio::select!` against
  `cancel.cancelled()`.
- On cancellation, return `ToolError::Aborted` and kill any child process
  started by Defuddle/headless code.

**Important:** Do not add new short wall-clock limits in this PR. The
only new stopping condition is explicit cancellation.

**Tests:**

- `web_read_honors_cancellation_before_fetch`
- `web_read_honors_cancellation_while_defuddle_running`
- `web_search_honors_cancellation_while_rate_limited_or_fetching`

Use fake runners/backends so tests are deterministic and do not require
real network or Node.

**Exit criteria:**

- Ctrl+C / `UiAction::Abort` can stop an in-flight web tool.
- No web tool ignores its `CancellationToken`.

### PR B — Bound side-channel reads

**Change:**

- Add a bounded byte-stream collector for HTTP bodies.
- Use it for:
  - non-2xx error bodies before `HttpStatus` excerpting;
  - `robots.txt` fetches;
  - any future small metadata fetches.
- Replace `wait_with_output()` with bounded stdout/stderr readers for
  Defuddle. If output exceeds cap, kill the child and surface a typed
  error with a truncated excerpt.

**Tests:**

- Huge 500 body does not allocate past cap and returns `HttpStatus` with
  truncated excerpt.
- Huge `robots.txt` is treated as unavailable or typed-too-large without
  allocating unbounded memory.
- Defuddle stderr flood is capped and child is killed.

**Exit criteria:**

- Every web-tool body read has an explicit memory cap.
- Existing `DEFAULT_MAX_BYTES` remains the success page cap.

### PR C — Central configurable web budgets

**Change:**

- Add a minimal `ToolsWebConfig` under `AnieConfig.tools.web`.
- Extend web-tool constructors to accept a `WebToolOptions` /
  `FetchOptions` built from config.
- Move existing constants into defaults for that options type.
- Document how persistent-agent operators can relax budgets.

Suggested validation rules:

- Byte caps must be above small safe minimums.
- Timeout fields must be positive when enabled.
- If supporting "disabled" timeouts, use an explicit documented value
  (`0` or string enum) and tests.

**Tests:**

- Config absent → old defaults.
- Config present → values reach `WebReadTool` and `WebSearchTool`.
- Invalid tiny/negative-equivalent values reject config load with a clear
  message.
- Timeout disabled/relaxed behavior is represented unambiguously.

**Exit criteria:**

- No new wall-clock budget is hidden at an arbitrary call site.
- Operators can tune web-tool budgets for persistent-agent deployments.

### PR D — Optional progress/heartbeat updates

**Change:**

- For long web reads, emit `ToolExecUpdate` progress messages at coarse
  milestones: fetching, rendering JS, extracting content, etc.
- Avoid high-frequency updates; one update per phase is enough.

**Tests:**

- Progress updates are forwarded through the existing tool-update path.
- Cancellation during a phase still wins.

**Exit criteria:**

- Long-running web operations are observable in the UI without imposing
  short timeouts.

## Test plan

- `cargo test -p anie-tools-web`
- `cargo test -p anie-tools-web --features headless` where practical.
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- Manual smoke:
  - Start `web_read` against a slow endpoint, abort, confirm the run
    returns promptly and no child process is left behind.
  - Run a normal `web_search` → `web_read` chain and confirm default
    behavior remains usable.

## Risks

- Killing subprocesses cross-platform can be subtle. Prefer Tokio child
  APIs where possible and add Unix/Windows notes if behavior differs.
- Config surface can grow too fast. Start with only the knobs needed for
  reviewed findings; do not expose every internal constant unless users
  need it.
- A total Defuddle timeout may be too aggressive for first-run `npx` on
  slow networks. Keep the default generous and configurable, and prefer
  cancellation + output caps as the first safety layer.

## Exit criteria

- Web tools are cancellable.
- Memory side channels are capped.
- Time budgets are centralized, documented, and configurable.
- Persistent-agent use cases can opt into longer/no total wall-clock
  budgets without code changes.

## Deferred

- General agent job supervision / resumable tool jobs. This plan makes
  web tools safe and cancellable; resumable long-running work is a later
  persistent-agent feature.
