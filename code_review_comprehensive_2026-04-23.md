# Comprehensive Code Review — 2026-04-23

**Branch:** `feat/provider-compat-blob`  *(22 commits ahead of prior review baseline)*
**Scope:** every crate and every file in the workspace.
**Ordering:** non-TUI crates first, TUI last (per request — that's the file under
active edit, and the rendering pipeline gets its own deep-dive section).

This report is a companion to the earlier reports, not a replacement:
`code_review_2026-04-19.md`, `code_review_followup_2026-04-19.md`,
`code_review_thorough_2026-04-19.md`, `code_review_system_2026-04-19.md`,
`code_review_modularity_2026-04-21.md`, and
`code_review_performance_2026-04-21.md`. Where those are still accurate, this
review does not re-litigate them; where the code has moved, this review notes
what's changed.

---

## Executive summary

The workspace is in **good health**. The three headline signals all pass:

| Signal | Result |
|---|---|
| `cargo check --workspace --all-targets` | clean, no warnings |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean, 0 warnings |
| `cargo test --workspace --no-fail-fast` | 898 passed / 0 failed / 2 ignored (28 test runs) |

Workspace shape: **12 crates, 126 Rust files, ~53,658 LOC**.

The clean clippy + test result is notable: the workspace already denies
`needless_borrow`, `redundant_clone`, `uninlined_format_args`, `manual_let_else`,
and warns on `unwrap_used` / `expect_used`. All of that compiles clean today, so
most "clippy-bait" concerns in prior reviews have been worked off.

**What's genuinely strong right now:**

- `anie-protocol` and `anie-provider` are small, idiomatic data/contract crates
  with no rough edges.
- `anie-tools` is the cleanest modularity pattern in the workspace — one tool
  per file, no cross-coupling.
- `anie-providers-builtin`'s streaming path is well-factored — the OpenAI
  streaming state machine and the (correctly existing, see correction below)
  `AnthropicStreamState` are both clean.
- `anie-auth` is production-grade: correct PKCE, safe refresh-race
  double-checking with file locks, defensive auth-file quarantine on corruption.
- `anie-cli`'s `runtime/` extraction is real progress — `SessionHandle`,
  `ConfigState`, `SystemPromptCache` each own coherent slices of state.
- `anie-tui` rendering has **three-tier caching** (block / flat / viewport
  slice) and a streaming-aware commit/tail split. It's smarter than it needs
  to be for current workloads.

**What still needs attention:**

1. `anie-session/src/lib.rs` is still a 3,031-line single-file monolith — the
   biggest merge-conflict magnet in the workspace.
2. `anie-tools` depends on `anie-agent` for the `Tool` trait — a
   wrong-direction dep. `anie-tools/Cargo.toml:26` confirms it.
3. `anie-tui/src/app.rs` (2,237 LOC), `overlays/onboarding.rs` (2,644 LOC),
   and `overlays/providers.rs` (1,606 LOC) are large and procedural. Overlays
   all repeat the same `dispatch_key/tick/render` boilerplate.
4. The rendering pipeline has a few clear optimization opportunities (see
   the deep-dive section): **selective cache invalidation on setting toggle**,
   **animated-block cache scoping**, and **per-fence syntect caching** are all
   small, local changes with visible payoff.
5. `anie-cli/src/runtime/config_state.rs:94, 154, 172` constructs a fresh
   `CredentialStore` three times in fallback chains where one would do.
6. Coverage gaps: no integration test for slash-command dispatch, no regression
   test for the urgent-paint fast path added by `docs/tui_input_responsiveness_fix_plan.md`.

No critical bugs were found.

---

## Live verification

Each of the following commands was executed against the current tree during
this review:

```
cargo check --workspace --all-targets              # Finished, exit 0, no warnings
cargo clippy --workspace --all-targets -- -D warnings  # Finished, exit 0, 0 warnings
cargo test  --workspace --no-fail-fast             # 898 passed, 0 failed, 2 ignored
```

Test totals are aggregated across all 28 test binaries in the workspace (unit
tests per crate + 5 files in `anie-integration-tests/tests/`). Ignored tests
appear to be network-gated live-OAuth probes; that should be confirmed in
follow-up but is consistent with the `anie-auth` test comments.

---

# Per-crate review

## `anie-protocol` — 614 LOC, 10 files

**Verdict: excellent — no issues found.**

Pure data-only leaf crate. Types split by domain: `content.rs`, `messages.rs`,
`events.rs`, `stop_reason.rs`, `stream.rs`, `tools.rs`, `usage.rs`, `time.rs`.
Depends only on `serde`; nothing depends upward.

- Consistent serde hygiene: `#[serde(default, skip_serializing_if = "Option::is_none")]`
  on all optional fields (e.g., `content.rs:22`, `messages.rs:42,58,72`).
- `#[serde(tag = "type")]` for discriminated variants. Good schema shape.
- Zero `unwrap` / `expect`, zero hot-path allocations.
- `StopReason` has four variants to pi's five — the `"length"` case is
  deliberately routed to `ProviderError::ResponseTruncated` instead. This is
  documented design, not drift.

Nothing to change.

---

## `anie-provider` — 1,345 LOC, 10 files

**Verdict: excellent — no issues found.**

Small, focused: `provider.rs` (trait), `model.rs` (catalog), `error.rs` (14
structured variants), `options.rs`, `api_kind.rs`, `thinking.rs`,
`registry.rs`, `mock.rs`. No cycles, no upward deps into builtins.

- `ProviderError` is well-designed; `retry_after_ms()` helper (`error.rs:134-142`)
  gives the retry machinery exactly the taxonomy it needs. This is meaningfully
  better than pi's regex-based error classification (`docs/anie_vs_pi_comparison.md`).
- `Model`'s forward-compat game is solid: `#[serde(default, skip_serializing_if = "is_default_compat")]`
  on `ModelCompat` (`model.rs:309-320`), conservative `ReplayCapabilities`
  defaults (`model.rs:327-329`).
- Comprehensive round-trip tests verify serde stability (`model.rs:383-586`).

Nothing to change.

---

## `anie-providers-builtin` — 7,505 LOC, 14 files

**Verdict: good — two sub-files are large but internally cohesive, one
correction to note.**

Top-level: `anthropic.rs` (1,579 LOC), `openai/` (four files, 2,890 LOC
combined), `model_discovery.rs` (1,494 LOC), `openrouter.rs` (443 LOC),
`local.rs` (336 LOC), plus shared `sse.rs`, `http.rs`, `util.rs`, `models.rs`.

### Correction to prior characterisation

A first-pass scan claimed "Anthropic provider has no separate state
machine like OpenAI's". **This is stale.** `AnthropicStreamState` exists at
`anthropic.rs:436` and is constructed at `anthropic.rs:235`. The state
machine pattern is consistent across both backends. No refactor is needed
on this point.

### Findings

- **`openai/mod.rs` (1,229 LOC)** owns the provider `Provider` impl, request
  building, and native-reasoning strategy dispatch. It's at the outer edge
  of what's comfortable in one file; the other three files in `openai/`
  (`streaming.rs`, `convert.rs`, `reasoning_strategy.rs`, `tagged_reasoning.rs`)
  show that splitting *is* possible.
- **`model_discovery.rs` (1,494 LOC)** combines the OpenAI/Anthropic/Ollama/
  Copilot discovery protocols + the TTL cache + request builders + tests.
  Splitting by protocol (one `discover_*()` per file, plus a shared
  `cache.rs`) would halve the file and narrow merge conflicts.
- `OpenAiStreamState` at `streaming.rs:47-349` is excellent — clear lifecycle,
  `std::mem::take()` for zero-copy moves (`streaming.rs:292,298,328,335`),
  defensive tool-call JSON fallback (`streaming.rs:302-310`).
- `ModelDiscoveryCache` uses `Arc<[ModelInfo]>` for zero-copy sharing
  (`model_discovery.rs:61-120`) — good.
- Minor: `model.clone()` in OpenAI `stream()` (`openai/mod.rs:383-386`) clones
  `Model` three times per stream. Cost is low (mostly small strings), but
  `Arc<Model>` would eliminate it if this ever shows in a profile.
- `reasoning_details` accumulation is gated on the model catalog's replay
  flag (`streaming.rs:127-135,322-331`). Silent drop if a new upstream adds
  reasoning fields before the catalog is updated — not a bug, but a
  failure mode worth documenting in the catalog-update runbook.
- `filter(|entry| entry.object.as_deref().unwrap_or("model") == "model")`
  at `model_discovery.rs:358` is a permissive default. Safe today, but
  newer OpenAI API versions may tighten this.

No critical bugs. Two files are on the "could split" list; not urgent.

---

## `anie-agent` — 2,870 LOC, 5 files

**Verdict: solid — two structural findings.**

Files: `agent_loop.rs` (the big one), `tool.rs`, `hooks.rs`, `tests.rs`, `lib.rs`.

### Wrong-direction dep (blocker for modularity work)

`anie-tools/Cargo.toml:26` declares `anie-agent.workspace = true`. The
`Tool` trait lives here, and tools import it. Every structural tool change
therefore pulls `anie-agent`'s orchestration code into its compile unit.

**Fix:** move the `Tool` trait + `ToolRegistry` contract into a small
`anie-tool-contract` crate that both `anie-tools` and `anie-agent` depend on.
This was flagged in `code_review_modularity_2026-04-21.md`; it's still
open.

### Other

- Hot-path clones in event construction (~60 `.clone()` in `agent_loop.rs`).
  Example: `tool_call.id.clone()` and `tool_call.arguments.clone()` at
  `agent_loop.rs:787-790,799,857-858` ship the same IDs through
  `ToolExecStart` and `ToolExecEnd`. A shared `Arc<ToolCallDescriptor>`
  would eliminate this. Not a perf crisis — LLM latency dominates tool
  dispatch cost — but hygienic.
- `sanitize_context_for_request` uses `Cow<'a, [Message]>` (`agent_loop.rs:944-969`),
  which is exactly the right pattern. Keep doing this.
- `cancel.child_token()` (`agent_loop.rs:874`) is correctly scoped per tool
  call. No race on cancellation.

---

## `anie-tools` — 3,329 LOC, 11 files

**Verdict: exemplary modularity, minor polish items.**

Files: one tool per file (`read.rs`, `write.rs`, `edit.rs`, `bash.rs`, `grep.rs`,
`find.rs`, `ls.rs`) + shared helpers (`shared.rs`, `file_mutation_queue.rs`,
`tests.rs`, `lib.rs`). `lib.rs` is a 25-line re-export surface.

**This is the pattern the rest of the workspace should imitate.** Adding a new
tool touches exactly one file.

Polish items:

- `find.rs:100-101`: chained `unwrap_or` — `usize::try_from(...).unwrap_or(X).unwrap_or(X)`
  is redundant. Collapse to a single fallback.
- `read.rs:74`: `u64::try_from(bytes.len()).unwrap_or(u64::MAX)` — slice
  length always fits in `u64` on every supported platform; direct cast is
  clearer.
- Test-only `.expect()` is gated by `#[cfg(test)]`. Good.

---

## `anie-session` — 3,031 LOC, **1 file**

**Verdict: functional, but the biggest structural liability in the repo.**

`src/lib.rs` holds: schema definitions, `SESSION_SCHEMA_VERSION` gate,
file-lock logic, JSONL parsing, append/fork, branch walks, context
reconstruction, compaction (including `CompactionDetails`), listing, and tests
— all in one module with **34 top-level items** (functions, structs, enums,
impls).

### What's correct

- Schema versioning is real: `CURRENT_SESSION_SCHEMA_VERSION = 4`, forward-compat
  gate rejects unknown future versions gracefully (`lib.rs:523-532`).
- `details: Option<CompactionDetails>` is correctly optional, with
  `#[serde(default, skip_serializing_if)]` (`lib.rs:153,260,265`). Pre-v4
  sessions still load.
- File locks are best-effort on NFS/WSL — falls back to a warning rather than
  refusing to open (`lib.rs:47-64`).
- Malformed JSONL lines log a warning and skip rather than abort
  (`lib.rs:1104-1107`).
- Token estimation uses a pi-matching hybrid strategy (seed from provider
  usage, heuristic fallback for tail) at `lib.rs:1126-1189`.

### What hurts

1. **Merge-conflict magnet.** Any of these tasks lands in the same file:
   - new persisted entry type,
   - resume/list behavior change,
   - compaction tweak,
   - context-builder change,
   - schema version bump.
2. **Repeated branch walks.** `get_branch(leaf_id)` is called three times per
   compaction (`lib.rs:621,768,835`), each walking the full entry list from
   the root. For a 10k-entry session, three walks per compaction is
   measurable. A cached branch or incremental walk would fix it.
3. **No `#[serde(default)]` discipline doc for evolving `CompactionDetails`.**
   The pattern is used correctly today, but there's no written rule for
   contributors — the next field added without defaults will break
   backward-compat silently.

### Recommended split

Mirrors what `code_review_modularity_2026-04-21.md` proposed; still the right
shape:

```
crates/anie-session/src/
  lib.rs            (re-exports + SessionManager)
  schema.rs         (entry types + CURRENT_SESSION_SCHEMA_VERSION + compat tests)
  storage.rs        (JSONL parse/append + file_lock + path logic)
  context_builder.rs (branch walk + message reconstruction)
  compaction.rs     (CompactionDetails + cut-point logic)
  tokens.rs         (hybrid estimation)
  tests.rs
```

This is the single highest-leverage refactor for parallel work.

---

## `anie-auth` — 5,684 LOC, 10 files

**Verdict: production-grade. One documented UX edge, otherwise no issues.**

Files: `lib.rs`, `oauth.rs` (shared trait + PKCE), five provider-specific
OAuth files (`anthropic_oauth.rs`, `github_copilot_oauth.rs`,
`google_antigravity_oauth.rs`, `google_gemini_cli_oauth.rs`,
`openai_codex_oauth.rs`), `callback.rs`, `refresh.rs`, `store.rs`.

### What's correct

- **PKCE is RFC 7636 compliant** (`oauth.rs:229-240`): 32 random bytes →
  base64url verifier, SHA-256 challenge. Test `oauth.rs:305-315` verifies
  verifier→challenge determinism.
- **Refresh-race safety.** `OAuthRefresher` uses the double-check pattern
  inside a per-provider file lock (`refresh.rs:196-237,272,285-305`). Two
  anie processes cannot rotate the same refresh token in parallel. The
  concurrent-process test at `refresh.rs:495-530` exercises this.
- **Defensive auth-file handling.** Unreadable `auth.json` is quarantined,
  not silently overwritten (`lib.rs:364-376`). Test at `lib.rs:610-649`.
- **No path traversal surface.** All paths flow from `dirs::home_dir()` or
  explicit config; no user input reaches `Path::join` unvalidated. Callback
  server binds to `127.0.0.1` with a fixed port per provider.
- **Shared helpers pull weight.** The five OAuth providers do *not* collapse
  to one implementation (each has a distinct token-exchange shape), but
  `oauth.rs` gives them PKCE + RFC 3339 time + the `OAuthProvider` trait,
  which eliminates the obvious copy-paste.

### Minor

- `refresh.rs:286-305`: the lock spin-loop sleeps 50ms between attempts
  rather than using exponential backoff. Fine at current contention
  (basically single-process) but worth fixing if we ever run a sidecar
  refresher.
- `refresh.rs:316`: `unwrap_or(30)` on a `u64→i64` conversion for
  `REFRESH_SAFETY_MARGIN` — the constant always fits. `expect(...)` would
  document intent better than a silent fallback.
- **Known UX edge.** If a provider has *both* an API key (keyring) and an
  OAuth credential (`auth.json`), `CredentialStore::get` returns the
  keyring value (`store.rs:76-77`), losing OAuth structure. Documented at
  `lib.rs:259-262`; could surprise a user who logs in via OAuth after
  setting an API key. Not a bug; a candidate for a startup warning.
- `OAuthProvider` registry is hardcoded in `lib.rs:289-298` (match arms over
  provider names). Five providers is fine; a sixth means editing three
  places. Macro or `linkme`-style collector would help if we add more.

### Coverage

Wiremock-based tests cover `begin_login` and `complete_login` for each
provider. Memory note referencing "three live-only bugs documented" is not
cross-referenced in the code; recommend linking commit SHAs in `docs/` so
the history is recoverable.

---

## `anie-config` — 1,300 LOC, 2 files

**Verdict: simple and correct.**

`lib.rs` + `mutation.rs`. Layered config: built-in defaults → `~/.anie/config.toml`
→ nearest project `.anie/config.toml` → CLI overrides. Merge order is correct
(`lib.rs:396-410`), merge is non-destructive (`lib.rs:541-602` — empty partials
don't clear populated fields).

- `model_explicitly_set` flag (`lib.rs:549`) prevents a partially-populated
  `[model]` section from silently inheriting leftover values from a parent
  layer. Good defensive check.
- Context-file discovery is filename-only (`AGENTS.md`, `CLAUDE.md` by
  default) with a per-file and total-bytes cap (`lib.rs:495,506-515`) — DoS-safe.
- `mutation.rs` (comment-preserving TOML edits) is cleanly separated so
  CLI write paths don't tangle with read-only loading.

Nothing to change.

---

## `anie-cli` — 6,589 LOC, 21 files

**Verdict: decomposition is real; remaining work is polish.**

Files:
- **Controller core:** `controller.rs` (1,072), `controller_tests.rs` (912),
  `bootstrap.rs`, `main.rs`, `lib.rs`.
- **Runtime state:** `runtime/mod.rs`, `runtime/config_state.rs` (289),
  `runtime/session_handle.rs` (194), `runtime/prompt_cache.rs` (111).
- **Mode dispatch:** `interactive_mode.rs`, `print_mode.rs`, `rpc.rs`.
- **Supporting:** `commands.rs` (522), `retry_policy.rs` (579),
  `model_catalog.rs` (784), `compaction.rs`, `onboarding.rs`,
  `login_command.rs`, `models_command.rs`, `runtime_state.rs`, `user_error.rs`.

### Progress since last review

The `runtime/` extraction is working:
- `SessionHandle` wraps `SessionManager` with directory context
  (`runtime/session_handle.rs`).
- `ConfigState` bundles config + runtime-state + current selections
  (`runtime/config_state.rs`).
- `SystemPromptCache` owns staleness detection (`runtime/prompt_cache.rs`).

### What's still coupled

- `ControllerState` carries nine fields (`controller.rs:587-600`) and still
  owns retry-on-overflow + compaction orchestration
  (`controller.rs:802-838,661-676,678-708,710-735`). A dedicated
  `RetryOrchestrator` or `CompactionOrchestrator` would pull ~150 LOC out
  of `controller.rs`.
- `maybe_auto_compact()` and `force_compact()` (`controller.rs:678-735`)
  overlap significantly; share a helper.

### Concrete issues

- **Triple `CredentialStore::new()`** in fallback chains:
  `runtime/config_state.rs:94,154,172`. Each constructor does a keyring +
  JSON scan. Pass `&CredentialStore` as a parameter instead. Runs on every
  `apply_session_overrides` call.
- **Config reload is expensive.** `controller.rs:901-922` re-probes every
  OAuth provider on `/reload`. Acceptable because it's user-triggered, but
  document it as such — a surprise on slow networks.
- **`retry_policy.rs` is well-tested.** 52 test cases at `lines 230-579`.
  Rate-limit floor at 2s (`line 40`) is defensive but costs a user with a
  sub-second reset. Acceptable trade-off; commented.
- **Print-mode event ordering.** `print_mode.rs:34-62` uses a `streamed_text`
  flag to suppress duplicate output between `TextDelta` and `MessageEnd`.
  There is no defensive check if events arrive out of order. `AgentLoop`
  guarantees order today; a regression there would silently double-print.

### Coverage gaps

`controller_tests.rs` (912 LOC) covers the prompt-run loop and mock-provider
plumbing. What's not tested through the controller:

- `UiAction` dispatch through `handle_action()` (`controller.rs:269-477`) —
  all 15+ variants.
- Slash-command routing (`CommandRegistry` integration).
- Session fork / switch / new.
- `print_mode.rs` and `rpc.rs` startup wiring (only unit tests hit
  `InteractiveController`).
- `UserCommandError` propagation through the loop.

---

## `anie-integration-tests` — 1,768 LOC, 5 test files

**Verdict: good spread, thin in spots.**

`tests/` contains:

- `agent_session.rs` (405) — session persistence / resume / compaction across runs
- `agent_tui.rs` (259) — TUI-controller wiring
- `config_wiring.rs` (96) — layered config resolution
- `provider_replay.rs` (367) — round-trip replay fidelity
- `session_resume.rs` (433) — cross-process resume safety

`src/helpers.rs` (201) factors test fixtures.

### Gaps

- **No slash-command integration test.** `/model`, `/thinking`, `/compact`,
  `/fork`, `/session list`, `/session <id>` all route through `handle_action()`
  — none exercised end-to-end. Regression risk: `controller.rs` refactors
  silently break a command with no test to catch it.
- **No OAuth live-gate test documented.** Two `#[ignore]`d tests in the
  full workspace run — these are probably live probes, but the ignore
  reason isn't labelled.
- **No print-mode / RPC-mode fixture.** The two non-TUI entry points only
  have unit-level coverage today.

---

# `anie-tui` — 19,623 LOC, 35 files *(reviewed last)*

This is the crate under active edit, so the review goes deeper. I've split
it into two halves: the **rendering pipeline** (output + markdown + syntect),
which is what the user explicitly asked about, and everything else (app,
input, overlays, widgets, tests).

Layout by size:

| File | LOC |
|---|---|
| `overlays/onboarding.rs` | 2,644 |
| `tests.rs` | 2,450 |
| `output.rs` | 2,433 |
| `app.rs` | 2,237 |
| `markdown/layout.rs` | 1,866 |
| `overlays/providers.rs` | 1,606 |
| `overlays/model_picker.rs` | 854 |
| `input.rs` | 758 |
| `autocomplete/popup.rs` | 440 |
| `autocomplete/command.rs` | 370 |
| `render_debug.rs` | 334 |
| `widgets/select_list.rs` | 318 |
| `terminal.rs` | 304 |
| *(others)* | < 300 each |

---

## Rendering pipeline — the deep dive

The user's central question: *are we rendering nicely formatted text in the
output field in the most efficient way?*

The answer: **the current design is genuinely smart — three-tier caching,
streaming-aware commit/tail split, global syntect cache — but has three
concrete leaks worth fixing, all small and local**. None of them are
urgent; none change the architecture.

### Pipeline: input → parse → layout → cache → paint

The data flow per frame:

```
AgentEvent / user input
    │
    ▼
OutputPane::add_block() / append_to_last_assistant()
    │   blocks vec grows; per-block cache slot cleared;
    │   flat_cache_valid = false
    ▼
OutputPane::render(frame)
    │
    ▼
rebuild_flat_cache() (if !flat_cache_valid or width changed or has_animated_blocks)
    │
    │   for each block index i:
    │     cache hit (correct width + settings)? ── clone Arc<Vec<Line>> ─┐
    │     cache miss?                                                     │
    │       block_lines() ── parse markdown (pulldown-cmark, fresh) ──┐   │
    │                        consume events in LineBuilder           │   │
    │                        emit code blocks → syntect highlight    │   │
    │                        wrap spans to width                     │   │
    │                        find_link_ranges over wrapped output    │   │
    │       store LineCache (width, lines Arc, links Arc) ───────────┘   │
    │                                                                    │
    │     append cache lines to flat_lines ──────────────────────────────┘
    ▼
slice flat_lines[scroll_offset .. +viewport_height]
    ▼
ratatui Paragraph renders slice
```

Citations for the three-tier cache:

- Block-level `LineCache` (width-keyed, `Arc<Vec<Line>>`): `output.rs:64-79`.
- Flat cache (`flat_lines` + `last_link_map`, whole-transcript):
  `output.rs:176-219,654-798`.
- Viewport slice passed to Paragraph: `output.rs:551-610`.

### Hot paths — per-frame vs per-event vs per-message

**Per-frame** (bounded by terminal refresh, typically 20–60 Hz):

- `render()` first hits the fast-path at `output.rs:647-652`:
  `flat_cache_valid && width unchanged && !has_animated_blocks()` → O(1)
  return (clone of the cached flat vec and slice it).
- If streaming is active, `has_animated_blocks()` (`output.rs:624`) returns
  true and the fast path is skipped. Flat rebuild walks all blocks
  (`rebuild_flat_cache()` at `output.rs:654-798`). Finalized blocks still
  hit their per-block cache (O(1) deref), so the *per-block* cost stays
  low, but the outer walk runs every frame.

**Per-event** (keystroke, agent event, tool result):

- Block mutation → `invalidate_last()` or `invalidate_at(i)` at
  `output.rs:319-331`. Clears one cache slot, sets `flat_cache_valid =
  false`.
- Streaming delta → `StreamingAssistantRender::append_delta()` at
  `output.rs:91-106`. Splits incoming text at `\n`:
  - Completed lines → `committed_text` (markdown-rendered, width-keyed
    cache).
  - Trailing partial line → `tail_text` (plain-text wrap only, not
    cached).

**Per-message** (assistant finalization):

- `finalize_last_assistant()` at `output.rs:406-423` replaces the streaming
  render with a finalized block, clears the `StreamingAssistantRender`
  slot, invalidates the block cache. Next render parses the full message
  through the markdown path.

### Streaming: does markdown re-parse on every chunk?

Short answer: **no — it re-parses on every `\n` boundary**, not per char.
Two-part strategy (`output.rs:82-139, 1024-1054`):

- **Tail text** (no newline yet): rendered as plain text with
  `wrap_text(tail_text, width, default_style)` at `output.rs:116`. Never
  hits `pulldown-cmark`. Zero parse cost per delta.
- **Committed text** (has newlines): full markdown pipeline runs at
  `output.rs:131`. Cache keyed by `(committed_width, markdown_enabled)`
  (`output.rs:125`). On each newline, cache is invalidated
  (`output.rs:104`) and re-parsed next render.

So for a 500-char response in 50-char chunks where newlines fall every
100 chars: chunks 1–9 all go to `tail_text` (no parse); chunk 10 commits
50 chars and triggers a parse; chunk 20 commits 100 chars and triggers a
parse of the full 100. **Cost is per newline, not per delta — good.**

### Is `pulldown-cmark` parsing incremental?

**No.** Every call to `render_markdown(text, width, theme)`
(`markdown/parser.rs:17-23` → `markdown/layout.rs:30-32`) constructs a
fresh `Parser::new_ext(text, options)` and consumes the full event stream
into `LineBuilder`. There is no AST caching, no event memoization, no
incremental diff.

**Cost:** `pulldown-cmark` is fast — expect <1 ms for 10 KB of markdown.
So this isn't a hot-path concern at current message sizes. But it does
mean:

- Every theme toggle re-parses every block.
- Every width change re-parses every block.
- Every `set_markdown_enabled(true/false)` call re-parses every block.

True incremental parsing would require either wrapping `pulldown-cmark`
with a custom diff layer or switching parsers; neither is justified by
profiles today.

### Is line wrapping cached separately?

**No — parse + wrap share one cache entry.** `LineCache`
(`output.rs:64-79`) stores the post-wrap `Arc<Vec<Line>>` keyed by
`width`. Wrapping happens inline inside `block_lines()` via
`wrap_spans()` (`markdown/layout.rs:1065-1143`).

Because width is baked into the cache key, any width change invalidates
every block's cache and forces a parse + wrap — even though the parse
output is width-independent and could have been reused.

### Syntect highlighting — how often, and is it cached?

**Global state is cached; per-fence output is not.**

- `SyntaxSet` + `ThemeSet` are loaded once via `OnceLock`
  (`markdown/syntax.rs:26-47`). This is correct and unchanged per process
  lifetime.
- Theme resolves to `base16-eighties.dark` with fallbacks
  (`markdown/syntax.rs:41-48`).
- **Per-fence highlighting re-runs every time the block cache misses.**
  `emit_code_block()` (`markdown/layout.rs:772`) constructs a fresh
  `HighlightLines::new(syntax, theme)` and re-highlights the code. On a
  theme change, every fence re-highlights. On a width change, every fence
  re-highlights. On any setting toggle, every fence re-highlights.

For typical sessions with a few code blocks this is negligible. For long
sessions with dozens of large fences, the theme-toggle case is the most
painful: O(total_fence_chars) work for an action the user probably didn't
realize was expensive.

### Bugs / edge cases (none critical)

1. **Animated-block cache scope** (`output.rs:616-618, 641-646`): a single
   streaming block forces `flat_cache_valid = false` for the whole
   transcript every frame. Finalized blocks still hit their per-block cache
   (cheap), but the outer walk runs. For a transcript with 600 finalized
   blocks + 1 streaming, that's 600 cache lookups and a full flat rebuild
   per tick. Fixable; see recommendation 2.
2. **Setting toggles over-invalidate** (`output.rs:263-305`): `set_markdown_enabled`,
   `set_tool_output_mode`, and `set_terminal_capabilities` all call
   `invalidate_all_caches()`. But `tool_output_mode` only affects tool
   blocks; `markdown_enabled` only affects user/assistant blocks;
   `capabilities` affects link rendering only. Selective invalidation is
   easy to write.
3. **Tail text is always plain** (`output.rs:115-117`): a half-typed
   header like `# Hello` renders as plain grey until the newline is
   committed, at which point it snaps to a styled heading. For LLM
   streams this is fine (they almost always end each structural line
   with `\n`). For hypothetical human-edit use cases it would look
   jarring.
4. **Link-URL paren handling** (`markdown/mod.rs:74-89`): extracts URL by
   scanning for `'('` / `')'`. A URL containing a nested `)` would be
   truncated. No real-world URLs do this; the code is correct for the
   stated format.
5. **Dead code:** `tick_autocomplete()` (`input.rs:160-162`) is now a
   no-op since the debounce was removed, but `app.rs:466` still calls it
   from the render loop. Harmless but removable. Test at `input.rs:747-751`
   pins the no-op.

### Recommendations (ordered by impact / effort)

| # | Change | Impact | Effort |
|---|---|---|---|
| 1 | **Lazy-invalidate on setting change.** `set_markdown_enabled` only invalidates user/assistant blocks. `set_tool_output_mode` only invalidates tool blocks. `set_terminal_capabilities` only invalidates blocks with links. | Medium | ~30 LOC |
| 2 | **Animated-block cache scope.** Track `animated_block_indices: HashSet<usize>`. Flat cache validity is independent of streaming presence; the streaming block is the only one that needs re-walking each frame. | Medium | ~50 LOC |
| 3 | **Per-fence syntect cache.** Keyed by `(code_hash, lang, theme_id)`, capped LRU. Eliminates re-highlighting on theme toggle and width change. | Low–medium | ~80 LOC |
| 4 | **Decouple parse cache from layout cache.** Store `Arc<Vec<Event>>` (from pulldown-cmark) keyed only by text. Store `Arc<Vec<Line>>` keyed by `(text, width, theme_id, markdown_enabled)`. Theme toggles and width changes keep the parse cache; they only re-layout. | High | ~200 LOC |
| 5 | **Streaming committed AST cache.** Stash the parsed event stream in `StreamingAssistantRender`; on newline boundary, re-parse only the *new* committed chunk and concatenate. | Medium | ~100 LOC. Only useful if #4 is done. |
| 6 | **Delete `tick_autocomplete()` and its render-loop call.** It's a no-op today. | Trivial | ~5 LOC |
| 7 | **Don't implement incremental `pulldown-cmark` parsing.** Cost/benefit doesn't justify it. | — | — |

**My recommendation: do 6, 1, 2 this week. Do 3 when we see a profile that
justifies it. Do 4 as a focused PR when the architecture merits it —
probably after `anie-session` is split, so it can be one clean change
instead of colliding with rendering-adjacent refactors.**

---

## TUI — everything else

### `app.rs` (2,237 LOC) — event loop, state, overlay router

The file still mixes UI composition, event dispatch, overlay routing,
command dispatch, and background-worker ownership. It works, but it's
the second-largest file in the crate and the boundary everyone edits.

- **Input responsiveness fix is partially landed** per `docs/tui_input_responsiveness_fix_plan.md`.
  `RenderMode::UrgentInput` exists (`app.rs:77-80,125`). `render_with_mode`
  gates transcript rebuild on `matches!(mode, RenderMode::UrgentInput)`
  (`app.rs:495`). No regression test yet — the plan calls for one.
- **State duplication risk:** `known_models: Vec<Model>` on `App`
  (`app.rs:58,328,1548-1556`) is mirrored by `discovery_cache:
  Arc<Mutex<ModelDiscoveryCache>>` (`app.rs:60,1526`) that overlays also
  read/write. Works today; vulnerable to drift if a future overlay
  forgets to push back.
- **StatusBar ownership is diffuse.** Multiple code paths poke
  `status_bar_mut()` (`app.rs:222-239,341-343,734-735`). No single owner
  sets `provider_name` / `model_name`. A small `StatusBarController`
  would centralise it.
- **Overlay extension is compile-time coupled.** Adding a new overlay
  means adding a variant to `OverlayOutcome` and a match arm in
  `apply_overlay_outcome` (`app.rs:1560-1687`). Not a trait; not
  pluggable.

### `input.rs` (758 LOC) — input handling & autocomplete trigger

- Autocomplete refreshes on every keystroke (`input.rs:145-151`), not from
  a tick. Test at `input.rs:730-740` pins one query per keystroke.
  Matches plan phase D.
- UTF-8 cursor handling is correct (`input.rs:666-690` uses
  `is_char_boundary`).
- Dead code: `tick_autocomplete()` no-op at `input.rs:160-162` (see
  rendering recommendation 6).

### `commands.rs` (282 LOC)

Clean data-only module. `SlashCommandInfo::validate` (`commands.rs:146-176`)
is case-insensitive for enum matching and tested across all argument
variants.

### `overlays/` — framework + six overlays

- `overlay.rs:44-53` defines a three-method trait (`dispatch_key`,
  `dispatch_tick`, `dispatch_render`). Simple and explicit.
- **Every overlay repeats the same adapter boilerplate** (~6 lines each ×
  6 overlays). Macro candidate.
- `overlays/onboarding.rs` (2,644 LOC) is a 15-state procedural FSM
  (`MainMenu`, `ProviderPreset`, `OAuthInstructions`, `CustomEndpoint`,
  `BusyValidating`, `PickingModel`, ...). Handlers at
  `onboarding.rs:611-880`. Works; not easy to change.
- `overlays/providers.rs` (1,606 LOC) runs three concurrent model-picker
  tasks (`providers.rs:311-396`). Stringly-typed provider lookup
  (`providers.rs:759,769`).
- `overlays/model_picker.rs` (854 LOC) is well-isolated; reused by both
  onboarding and providers.

### `autocomplete/` (~1,100 LOC)

Well-tested. `parse_context` (`mod.rs`) decides command-name vs argument
completion (25+ test cases). `CommandCompletionProvider` reads the command
catalog handed down by `App::new`. No ambient state.

### `widgets/` — `select_list.rs`, `text_field.rs`, `fuzzy.rs`, `panel.rs`

All small, boring, correct.

### `terminal.rs` + `terminal_capabilities.rs`

`setup_terminal`, `restore_terminal`, RAII `TerminalGuard`, synchronized
output wrapping — all correct. `TerminalCapabilities` covers Kitty image
protocol, sixel, hyperlink support; plumbed to `OutputPane`.

### `tests.rs` (2,450 LOC, ~65 test cases)

Good flow coverage: streaming deltas, tool calls, compaction, history
navigation, autocomplete firing, slash-command dispatch, overlay routing.
Gaps:

- No regression test for urgent-paint fast path
  (`RenderMode::UrgentInput` skipping transcript rebuild).
- No `RESIZE_DEBOUNCE` or frame-coalescing test.
- No `STREAM_STALL_WINDOW` stall-detection test.
- No mouse hit-test coverage.

---

# Cross-cutting findings

## Code patterns

1. **Clippy is already clean.** Workspace lints
   (`Cargo.toml:87-93`) deny `needless_borrow`, `redundant_clone`,
   `uninlined_format_args`, `manual_let_else`, and warn on
   `unwrap_used` / `expect_used`. All compile clean today. Future work
   should keep this bar.
2. **Stringly-typed IDs appear in the TUI** (`provider_name: String`,
   `ModelId` as `&str`, `SetModel(String)`). No correctness bugs, but
   typo-fragile. Newtypes (`ProviderId`, `ModelId`) with
   `#[derive(Display, AsRef<str>)]` would cost little and catch
   mismatches at compile time.
3. **`.clone()` on hot paths.** Mostly small string clones for event
   construction. Replacing with `Arc<str>` would need profiling to
   justify.

## Modularity priorities (in order)

1. **Split `anie-session/src/lib.rs`.** The single biggest win for
   parallel work.
2. **Move `Tool` trait to `anie-tool-contract` crate.** Fixes the
   anie-tools → anie-agent wrong-direction dep.
3. **Split `overlays/onboarding.rs` (2,644 LOC).** Per-state modules +
   an FSM runner.
4. **Split `overlays/providers.rs` (1,606 LOC).** Similar pattern.
5. **Extract a `RetryOrchestrator` / `CompactionOrchestrator`** out of
   `controller.rs`.
6. **Split `model_discovery.rs` (1,494 LOC)** by protocol.

## Performance

No crisis. The five changes with the clearest payoff (all small, all
local):

1. `CredentialStore` de-duplication in `runtime/config_state.rs`.
2. Animated-block cache scoping in `output.rs`.
3. Lazy cache invalidation on setting changes in `output.rs`.
4. Cached branch walk / incremental walk in `anie-session`.
5. Per-fence syntect cache in `markdown/layout.rs`.

## Testing

Coverage has one systemic hole: **end-to-end slash-command routing
through the controller has no test**. Every other layer of the stack has
tests; the dispatch layer itself does not. Recommended single test:
script a controller run that dispatches `/model`, `/thinking`,
`/compact`, `/fork`, asserts each action fires and state updates.
~100 LOC, high value.

---

# Current status of the app

- **Builds clean.** No warnings across workspace check.
- **Lints clean.** 0 warnings under `clippy -- -D warnings` with the
  strict workspace lint set.
- **Tests green.** 898 passed / 0 failed / 2 ignored.
- **OAuth ships.** Five providers implemented and integration-tested.
- **Rendering is correct.** Three-tier cache is working and smart; the
  open items are optimisation polish, not correctness fixes.
- **TUI input responsiveness.** Plan is live, Phase D (no-debounce
  autocomplete) and `UrgentInput` paint mode are landed; regression test
  still missing.
- **Runtime decomposition.** `anie-cli/runtime/` is working; further
  controller splits are candidate next steps.
- **Documentation quality.** `docs/` is well-kept; plan folders and
  comparison docs are the right shape. `code_review_modularity_2026-04-21.md`
  and `code_review_performance_2026-04-21.md` remain largely accurate —
  nothing in their findings has been invalidated.

No show-stoppers. The work queue is polish, structural cleanup, and the
rendering-pipeline micro-optimisations enumerated above.
