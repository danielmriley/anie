# Comprehensive code review — 2026-04-27

Reviewed current `main` at `f071c53` (`web_tool/PR8: nudge agent to actually use web tools`). Focus areas were the weekend changes around native web tools, Ollama native `/api/chat` and context controls, TUI performance/polish, config/runtime persistence, auth hardening, and tool resource caps.

Pre-existing untracked `docs/code_review_2026-04-24/` was left untouched.

## Validation run

| Command | Result | Notes |
|---|---:|---|
| `cargo test --workspace` | ✅ pass | Workspace unit/integration/doc tests passed. |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ pass | No clippy warnings. |
| `cargo check -p anie-tools-web --features headless` | ✅ pass | Headless Chrome feature compiles. |
| `cargo check -p anie-cli --features web-headless` | ✅ pass | CLI feature wiring compiles. |
| `cargo fmt --all -- --check` | ❌ fail | Multiple files are not rustfmt-formatted; this will fail the `fmt` CI job. |

## Findings

### 1. High — `web_read` SSRF guard does not actually protect DNS/redirect/headless fetches

**Files:**
- `crates/anie-tools-web/src/read/fetch.rs:53-74`
- `crates/anie-tools-web/src/read/fetch.rs:308-315`
- `crates/anie-tools-web/src/read/fetch.rs:334-350`
- `crates/anie-tools-web/src/read/tool.rs:93-115`
- `crates/anie-tools-web/src/read/headless.rs:82-155`

`validate_url()` only checks the textual host (`host_str_is_private`). The comment says DNS hostnames resolving to private IPs are re-checked at fetch time, but the fetch path never inspects resolved socket IPs. A public-looking hostname can therefore resolve to `127.0.0.1`, `169.254.169.254`, RFC1918 space, etc. and pass validation.

Redirect handling has a second gap: `reqwest::redirect::Policy::limited` follows redirects before `fetch_html()` checks `response.url().host_str()`. If an allowed URL redirects to a private literal, the private request has already been sent by the time the code returns `PrivateAddress`.

The `javascript=true` path is broader still: after the initial textual validation and robots check, Chrome navigates the page itself. Chrome can follow redirects and load subresources without the Rust SSRF checks or byte caps.

**Impact:** Default-enabled web tools can be induced to send requests to loopback, cloud metadata, or intranet addresses. This is the highest-risk issue in the weekend web-tool work.

**Recommendation:** Disable automatic redirects and implement a manual redirect loop that validates each `Location` before the next request. Add DNS resolution/IP classification before connect, or use a connector/resolver strategy that rejects private resolved IPs at connect time. Include IPv4-mapped IPv6 (`::ffff:127.0.0.1`) in `ip_is_private`. Treat the headless path as a separate security boundary: either keep it disabled by default with stronger warnings, run Chrome with request interception that aborts private destinations, or explicitly document that `javascript=true` is not SSRF-safe.

---

### 2. High — `[ui]` config is ignored by the real config loader

**Files:**
- `crates/anie-config/src/lib.rs:41-43`
- `crates/anie-config/src/lib.rs:140-163`
- `crates/anie-config/src/lib.rs:699-780`
- `crates/anie-config/src/lib.rs:795-809`
- `crates/anie-cli/src/interactive_mode.rs:35-53`

`AnieConfig` has `ui: UiConfig`, and interactive startup reads `state.config.anie_config().ui` to configure autocomplete, markdown, and tool-output mode. But `load_config_with_paths()` parses into `PartialAnieConfig`, and that partial shape has no `ui` field. `merge_partial_config()` consequently never copies any `[ui]` values from TOML.

The current tests deserialize `UiConfig` directly, which proves the struct shape, but they do not exercise `load_config_with_paths()` with a `[ui]` table. User-facing settings such as these are silently stuck at defaults:

```toml
[ui]
slash_command_popup_enabled = false
markdown_enabled = false
tool_output_mode = "compact"
```

**Impact:** Documented UI preferences do not work from config files. This affects older `slash_command_popup_enabled` / `markdown_enabled` settings and the newly added `tool_output_mode`.

**Recommendation:** Add `ui: Option<PartialUiConfig>` to `PartialAnieConfig`, merge each optional field into `config.ui`, and add a regression test through `load_config_with_paths()` for all three fields. Also consider adding a commented `[ui]` section to `default_config_template()` for discoverability.

---

### 3. High — Formatting is currently red in CI

**Files:** many rustfmt diffs reported by `cargo fmt --all -- --check`, including:
- `crates/anie-cli/src/controller.rs`
- `crates/anie-cli/src/controller_tests.rs`
- `crates/anie-provider/src/model.rs`
- `crates/anie-providers-builtin/src/local.rs`
- `crates/anie-providers-builtin/src/ollama_chat/mod.rs`
- `crates/anie-tools-web/src/read/*.rs`
- `crates/anie-tools-web/src/search/*.rs`
- `crates/anie-tui/src/app.rs`
- `crates/anie-tui/src/input.rs`
- `crates/anie-tui/src/output.rs`
- `crates/anie-tui/src/overlays/onboarding.rs`

The repository has a `fmt` CI job (`.github/workflows/ci.yml`) that runs `cargo fmt --all -- --check`, and that command fails locally right now.

**Impact:** CI will fail despite tests and clippy passing.

**Recommendation:** Run `cargo fmt --all` and commit the formatting-only diff separately from behavior changes.

---

### 4. Medium — Web tools ignore cancellation, and Defuddle has no execution timeout

**Files:**
- `crates/anie-tools-web/src/read/tool.rs:167-180`
- `crates/anie-tools-web/src/search/tool.rs:123-136`
- `crates/anie-tools-web/src/read/extract.rs:150-165`
- `crates/anie-agent/src/agent_loop.rs:892-900`

`AgentLoop` passes a child `CancellationToken` to tools, then awaits the tool future. `web_read` and `web_search` discard that token (`_cancel`) and call `run()` directly. Inside `web_read`, the Defuddle subprocess is awaited with `child.wait_with_output().await` and no timeout. If `npx`, Defuddle, DNS, a slow fetch, or the rate limiter stalls, Ctrl+C / abort cannot interrupt the tool promptly.

**Impact:** A single web tool call can keep an agent run alive after the user aborts. The Defuddle path is the most concerning because `npx defuddle@0.18.x` can involve package resolution/install work and unbounded subprocess runtime.

**Recommendation:** Thread `CancellationToken` through `run()`, `fetch_html`, the rate-limiter wait, headless rendering, and the Defuddle runner. Wrap external subprocesses in `tokio::select!` against cancellation and a dedicated timeout; kill the child on either path. Add tests proving `web_read` and `web_search` return `ToolError::Aborted` quickly when the token is cancelled.

---

### 5. Medium — “Bounded” web fetches still have unbounded error/side-channel reads

**Files:**
- `crates/anie-tools-web/src/read/fetch.rs:334-341`
- `crates/anie-tools-web/src/read/fetch.rs:214-224`
- `crates/anie-tools-web/src/read/extract.rs:160-169`

The success body is streamed and capped by `opts.max_bytes`, but several adjacent paths still read entire bodies into memory:

- non-2xx HTTP responses call `response.text().await` before truncating the excerpt;
- `robots.txt` uses `response.bytes().await` with no robots-size cap;
- Defuddle uses `wait_with_output()`, collecting all stdout/stderr before checking status.

**Impact:** A hostile or broken server can return a huge 404/500 body and bypass the 10 MiB page cap. A bad Defuddle/npx invocation can also fill memory with stderr/stdout.

**Recommendation:** Use the same streaming cap helper for success and error bodies, add a small robots cap (for example 512 KiB), and replace `wait_with_output()` with bounded stdout/stderr readers plus a subprocess timeout.

---

### 6. Medium — Built-in `read` still loads full files before truncating

**Files:**
- `crates/anie-tools/src/read.rs:58-72`
- `crates/anie-tools/src/read.rs:97-116`

The read tool advertises 50 KiB / 2000-line output truncation, but it first calls `tokio::fs::read(&abs_path)` and converts the whole file to a `String`. The image path has a size cap after reading, but text files have no pre-read size limit.

**Impact:** Asking to read a multi-GB file can allocate the whole file before truncation. The weekend edit-resource caps are good, but the read path remains an easy memory pressure vector.

**Recommendation:** Check metadata before reading and/or stream via `BufReader`, stopping after the requested offset/limit and byte cap. If full line counts are needed for the footer, either compute them with a streaming pass under a separate max-file-size cap or make the footer approximate when the file is too large.

---

### 7. Medium — Ollama load-failure recovery message uses the wrong requested `num_ctx` when an override is active

**Files:**
- `crates/anie-cli/src/controller.rs:194-210`
- `crates/anie-cli/src/user_error.rs:54-73`
- `crates/anie-cli/src/runtime/config_state.rs:92-95`

On `ModelLoadResources`, the controller renders a rich message by passing `model.context_window` as the requested `num_ctx`. But the actual wire request uses `effective_ollama_context_window()`, which can be a per-model runtime override. If a user has run `/context-length 65536` on a model whose discovered context is 262144, a failure message will incorrectly say the failed values were 262144 and 131072, while the actual attempts were 65536 and 32768.

**Impact:** The recovery hint can be misleading exactly for users who are actively tuning context length.

**Recommendation:** Pass `self.state.config.effective_ollama_context_window()` into `render_user_facing_provider_error()`. Add a controller/user-error test with an active runtime override.

---

### 8. Medium — robots.txt evaluation ignores the caller’s user-agent and caches too coarsely

**Files:**
- `crates/anie-tools-web/src/read/fetch.rs:153-179`
- `crates/anie-tools-web/src/read/fetch.rs:182-197`
- `crates/anie-tools-web/src/read/fetch.rs:208-224`

`RobotsCache::check()` accepts `user_agent`, but `evaluate()` names it `_user_agent`, and `fetch_robots_for()` always constructs `Robot::new("*", &body)`. The cache key is just `host`, not origin (`scheme`, host, port) and not user-agent.

**Impact:** Sites with `User-agent: anie` rules can be evaluated under the wildcard group instead. The host-only cache can also reuse a policy across different ports/schemes on the same host.

**Recommendation:** Construct `Robot` for the configured user-agent (or cache parsed robots text and evaluate per user-agent), and key the cache by origin. Add tests where `User-agent: anie` differs from `User-agent: *`.

---

### 9. Low — Defuddle receives a tempfile path but the original source URL is unused

**Files:**
- `crates/anie-tools-web/src/read/extract.rs:104-114`
- `crates/anie-tools-web/src/read/extract.rs:123-128`
- `crates/anie-tools-web/src/read/extract.rs:150-155`

The `DefuddleRunner` trait says `source_url` is provided for relative-link resolution and metadata, but `SubprocessDefuddleRunner::run()` takes `_source_url` and never passes it to the CLI. Defuddle parses a local tempfile path instead.

**Impact:** Relative links/images in extracted Markdown may resolve as file-relative or remain unresolved rather than being based on the original page URL. The YAML frontmatter still records the source URL, but the extractor itself does not see it.

**Recommendation:** If Defuddle 0.18 supports a base/source URL flag, pass it. If it does not, document the limitation and consider post-processing relative links against the original URL.

---

### 10. Low — `atomic_write()` is atomic for replacement but not fully crash-durable

**Files:**
- `crates/anie-config/src/lib.rs:318-356`

`atomic_write()` writes and fsyncs the temp file, then renames it over the destination. It does not fsync the containing directory after the rename. On POSIX, that means a crash immediately after rename can still lose the directory-entry update on some filesystems/mount options.

**Impact:** The helper is safer than direct writes and should preserve the old file on write failures, but its documentation overstates crash durability for config/auth/runtime-state persistence.

**Recommendation:** After a successful rename, open the parent directory and `sync_all()` it on Unix platforms that support directory fsync. Alternatively, weaken the durability claim in the doc comment.

## Positive notes

- The test suite is broad and fast; workspace tests and clippy are green.
- The Ollama native `/api/chat` path has good typed-error integration and strong unit coverage around `num_ctx`, native reasoning fallback, and load-failure classification.
- The TUI cache work is careful about hot paths (`Arc<Line>`, block/flat cache split, bounded event drains) and has regression tests for streaming markdown and cache behavior.
- OAuth refresh locking correctly moved blocking lock acquisition off Tokio workers and has contention tests.
- The new web tools have a solid modular shape (`fetch`, `extract`, `frontmatter`, `search`) and typed errors; the remaining issues are mostly boundary hardening.

## Suggested fix order

1. Run `cargo fmt --all` to unblock CI.
2. Fix the web SSRF boundary before relying on `web_read` in normal operation.
3. Restore `[ui]` config loading with end-to-end config tests.
4. Add cancellation/timeouts and bounded side-channel reads to web tools.
5. Fix the Ollama `num_ctx` message to use the effective context window.
6. Stream/cap the built-in `read` tool input path.
