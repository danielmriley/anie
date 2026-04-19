# anie — Project Review (2026-04-17)

Reviewer's framing: this is a coding-agent harness that has grown past
the scaffolding phase. It compiles clean, `cargo clippy --workspace
--all-targets` is warning-free, and the test suite passes. The
architecture doc in `docs/arch/anie-rs_architecture.md` is accurate and
the dependency graph has no cycles. None of what follows is about
correctness of what exists — it's about the shape of what's there, and
the cost of extending it.

The executive summary: the **crate boundaries are good**, the
**per-crate internals have drifted**. Several files have grown into God
objects that should have been split two commits ago, and there is a
specific pattern of duplication between the two TUI overlays and
between the two HTTP providers that will keep compounding until it's
addressed. Priorities are listed at the end.

---

## 1. What's working well

- **Clean crate graph.** `anie-cli` is the only thick consumer; the rest
  form a small DAG with no cycles. `anie-protocol` and `anie-provider`
  are correctly at the bottom. This is the kind of structure that makes
  future refactors tractable.
- **Protocol types are tidy.** `anie-protocol/src/content.rs` uses
  tagged enums with no `Unknown` variant, no `Option<Vec<T>>` smells,
  serde-clean. Good foundation.
- **Provider trait is small.** `Provider` has three required methods
  plus one optional (`includes_thinking_in_replay`), and the
  `ProviderRegistry` dispatches by `ApiKind`. This is an appropriately
  narrow contract.
- **`FileMutationQueue`** (`crates/anie-tools/src/file_mutation_queue.rs`)
  is a genuinely nice piece of engineering: per-canonical-path
  `DashMap<PathBuf, Arc<Mutex<()>>>` gives serialized writes without a
  global lock, and `canonicalize_best_effort` handles the
  "parent-doesn't-exist-yet" case correctly.
- **Auth resolution is centralized.** `anie-auth/src/lib.rs`'s
  `AuthResolver::resolve()` is the single source of truth for the
  CLI → keyring → JSON → env-var order. Providers don't re-implement
  it. The legacy `auth.json` → `auth.json.migrated` migration is
  idempotent.
- **Config layering is correct.** Global → project → CLI merge is
  field-by-field optional merge; `ConfigMutator` (`mutation.rs`) uses
  `toml_edit::DocumentMut` so comments and formatting survive writes.
  This is the right call.
- **Session JSONL is append-only with graceful recovery.**
  `parse_session_file()` skips malformed lines with a `warn!` rather
  than erroring out; fork and compaction logic reads cleanly.
- **Workspace lints are strict.** `unwrap_used`/`expect_used` warn and
  `redundant_clone`/`uninlined_format_args`/`manual_let_else` deny at
  the workspace level. Release profile is tuned (`lto = "fat"`,
  `opt-level = "z"`, `codegen-units = 1`, `panic = "abort"`).
- **`docs/reasoning_fix_plan.md`** is exactly the right kind of
  document: it names the bug, names the three independent behaviors
  that combine to produce it, and sequences the fix as three explicit
  phases with file-level diffs. Keep writing plans like this.

---

## 2. Critical issues

### 2.1 Monolithic files have outgrown their modules

Your own CLAUDE.md flags files over ~300 LOC as refactor candidates.
The workspace has several that are 3–7× that:

| File | LOC | Observation |
|---|---|---|
| `crates/anie-tui/src/onboarding.rs` | 2312 | 11-variant `OnboardingState` enum dispatched through giant match arms |
| `crates/anie-providers-builtin/src/openai.rs` | 2084 | Request building + SSE parsing + tool reassembly + reasoning splitter all in one file |
| `crates/anie-cli/src/controller.rs` | 1967 | `ControllerState` God object, ~20 methods |
| `crates/anie-session/src/lib.rs` | 1474 | All of session state, fork logic, compaction, and parsing in one module |
| `crates/anie-tui/src/providers.rs` | 1432 | Parallels onboarding.rs structure and duplicates much of it |
| `crates/anie-tui/src/app.rs` | 1329 | `handle_agent_event()` is a 134-line inline match |

None of these are irredeemable, but each one is now a place where
editing is risky because you have to hold too much in your head at
once. Section 4 proposes concrete splits.

### 2.2 Duplication between the two TUI overlay screens

`onboarding.rs` and `providers.rs` are parallel implementations of
"full-screen overlay that edits provider config." They have
independently diverged and now share:

- **`struct TextField`** — identical impls at `onboarding.rs:169` and
  `providers.rs:120`. A UTF-8 cursor fix would need to land twice.
- **`render_status_panel` / `render_busy_panel`** — `onboarding.rs:1409`
  vs. `providers.rs:630`. Same layout, same colors, slight drift.
- **`centered_rect` / `footer_line`** — defined in onboarding, used in
  both; providers has its own near-copies.
- **Model-picker embedding** — both screens wrap `ModelPickerPane` with
  near-identical overlay chrome.

The root cause is that there is no `OverlayScreen` trait. `App` holds
an `OverlayState` enum and matches on it for both event dispatch and
render. Adding a third overlay (a `/settings` screen from `ideas.md`)
will multiply this cost.

### 2.3 Duplication between the two HTTP providers

`anthropic.rs` and `openai.rs` both build auth headers, both do
status-code → `ProviderError` classification, both run retry loops,
both accumulate tool-call state, both consume SSE from
`eventsource-stream`, and each does it slightly differently:

- OpenAI uses `bearer_auth` (`openai.rs:148-161`); Anthropic sets
  `x-api-key` manually (`anthropic.rs:108-119`). Custom-header loops
  are otherwise identical.
- Tool-call reassembly: OpenAI tracks `started` / `ended` booleans
  (`openai.rs:1025-1026`); Anthropic leaves `None` in a blocks map
  (`anthropic.rs:488-493`). Same problem, two shapes.
- JSON parse errors inside the event loop are mapped to
  `ProviderError::Stream(&str)` in both places, with the error message
  as the only distinguishing signal. Callers then `contains("empty
  assistant response")` or similar string-match to decide whether to
  retry. This is fragile.
- `model_discovery.rs` has three parallel `discover_*` functions
  (OpenAI-compatible, Anthropic, Ollama) that each construct a fresh
  `reqwest::Client` and dispatch auth. A single function keyed on
  `ApiKind` would absorb ~200 LOC.

### 2.4 `ControllerState` is a God object

`crates/anie-cli/src/controller.rs` has `ControllerState` owning session
management, model resolution, compaction trigger logic, tool registry
construction, runtime state persistence, auth resolution, retry
bookkeeping, and system-prompt assembly. Concretely:

- Model resolution is spread across four functions (`resolve_model`,
  `resolve_requested_model`, `resolve_initial_selection`,
  `fallback_model_from_provider`).
- Compaction code is duplicated between `maybe_auto_compact`,
  `force_compact`, and `retry_after_overflow` (~80% overlap).
- The slash-command dispatcher (`handle_action`, lines 426–591) is a
  flat 20-arm match. Help text and command routing are not derived
  from one source; adding `/settings` or `/copy` from `docs/ideas.md`
  means editing this function and the help text separately.
- The provider registry is passed deep into
  `session.auto_compact()` / `session.force_compact()` — `anie-session`
  should not need to know about providers. This is a layering
  violation.

### 2.5 No unit tests for the streaming / tool-reassembly state machines

`openai.rs` and `anthropic.rs` have zero `#[cfg(test)] mod tests`.
Integration tests in `anie-integration-tests` exercise these paths
end-to-end, but:

- The `TaggedReasoningSplitter` (`openai.rs:646-759`) — a hand-rolled
  character-level state machine for `<think>` / `<thinking>` /
  `<reasoning>` tags — has no direct test coverage. It's exactly the
  kind of code that should be unit-tested on partial UTF-8 boundaries,
  interleaved tags, and malformed open/close sequences.
- `OpenAiStreamState::has_meaningful_content()` and
  `assistant_message_to_openai_llm_message()` are the load-bearing
  functions in `docs/reasoning_fix_plan.md`. Those sites currently
  have one test each; they deserve a full matrix.
- Tool-call reassembly is the kind of logic that breaks silently when
  a provider tweaks its streaming shape. Integration tests catch that
  expensively; unit tests catch it immediately.

### 2.6 No test coverage for the TUI overlays

`crates/anie-tui/src/tests.rs` (1118 lines) exercises `App`,
`OutputPane`, and `InputPane` well. It has **zero** tests for
`OnboardingScreen`, `ProviderManagementScreen`, or `ModelPickerPane`.
Given that these screens contain the two largest files in the TUI
crate, any refactor of sections 2.1 or 2.2 is currently unsafe.

### 2.7 `anie-extensions` is a placeholder

`crates/anie-extensions/src/lib.rs` is four lines — a `CRATE_NAME`
const and a doc comment. The architecture doc describes an `Extension`
trait with `before_agent_start`, `session_start`, and
`before/after_tool_call` hooks. None of it exists. The `hooks.rs` file
in `anie-agent` defines the traits but they're constructed as `None`
in the controller (`agent_loop.rs` config sites).

Two options: implement enough of the extension contract to make the
crate real, or delete it and revive the name when the contract is
designed. Leaving it as a stub communicates "this is wired up" when
nothing is.

### 2.8 CI does not enforce what the Makefile enforces

`.github/workflows/ci.yml` runs `cargo build --release` and `cargo
test --workspace`. It does **not** run `cargo clippy --workspace
--all-targets -- -D warnings` or `cargo fmt --all -- --check`, both of
which are in the `Makefile`. The workspace lint config is strict, but
CI will let a warning-introducing PR merge.

---

## 3. Medium-severity issues

### 3.1 Clone-heavy state in the TUI

`onboarding.rs:233` and `:284` clone the entire `OnboardingState` on
every tick/render cycle. `app.rs:728` and `:732` clone `Model` inside
decision trees where a borrow would work. `providers.rs:218` clones
`ProviderManagementMode`. This is not a perf emergency — the loop
runs at ~30fps with small data — but it's the kind of thing that
inflates allocator pressure and signals that state mutation is
fighting the borrow checker. Most of these are fixable with `&self.
state` in match arms.

### 3.2 `String`-keyed maps for typed data

`providers.rs:132` uses `test_results: HashMap<String, TestResult>`
keyed on provider name. A typo or case drift produces a silent miss.
A `HashMap<ProviderId, TestResult>` with `ProviderId(String)` newtype,
or a `HashMap<usize, _>` keyed by row index, would close this hole.

### 3.3 Slash command dispatch is not extensible

`ideas.md` lists nine more slash commands (`/settings`, `/copy`,
`/resume`, `/new`, `/name`, `/session`, `/tree`, `/export`, `/share`).
The current dispatcher is a flat match statement with hard-coded help
text. Moving to a command-registry pattern
(`HashMap<&'static str, Box<dyn CommandHandler>>` or a static slice)
would make help text, `/help` output, and the autocompletion
drop-down in `ideas.md` all derive from one source.

### 3.4 Error taxonomy is loose

`ProviderError` has `Auth`, `RateLimited`, `ContextOverflow`, `Http`,
`Request`, `Stream`, `Other`, `Response`. In practice:

- Construction is ad-hoc — `openai.rs:950` emits
  `ProviderError::Stream("empty assistant response")`, which callers
  pattern-match on. That's a string API.
- `ProviderError::Request` and `ProviderError::Other` absorb a wide
  range of causes.
- There's no `ProviderError::ToolCallMalformed`,
  `ProviderError::InvalidJson`, or `ProviderError::Timeout`, even
  though the code clearly distinguishes these at the call sites.

Tighten the enum to match how it's actually used, delete string-based
discrimination.

### 3.5 Scattered reasoning-capability logic

`default_local_reasoning_capabilities()` in `local.rs:61-87`,
`effective_reasoning_capabilities()` in `openai.rs:532-538`, and the
native-reasoning strategy selection in `openai.rs:236-277` are three
sites that together decide how thinking is requested and parsed. Phase
3 of `docs/reasoning_fix_plan.md` already proposes pulling these into
a declarative model profile; the plan is correct. Track it.

### 3.6 Session writes are unlocked

`anie-session` assumes a single writer but doesn't enforce it. Two
`anie` processes running in the same directory with the same `--resume
<id>` would interleave appends and corrupt the JSONL. Graceful
skip-on-parse-error means corruption is silent. Either `fd-lock` the
session file on open, or document the assumption loudly in the README
and log a warning on suspicious timestamps.

### 3.7 Silent `let _ = event_tx.send(...)`

`agent_loop.rs:343-348` and several sites in `controller.rs` swallow
channel send failures. In the normal case the channel is fine; in the
abnormal case (consumer dropped), every subsequent send is lost with
no log. At minimum log at WARN on the first failure per run.

### 3.8 Tool registry rebuilt per run

`controller.rs:969-984` constructs the full `ToolRegistry` on every
agent run. The registry is immutable — build it once, cache in
`ControllerState`, pass `Arc`-clones to each `AgentLoop`.

### 3.9 `.expect()` / `.unwrap()` in hot paths

Workspace lint says warn; you're not over the line, but a few sites
stand out:

- `model_picker.rs:542, 562` — `expect("selected model")` — should be
  a proper error path (the index can lag the backing vec on
  refresh).
- `http.rs:10`, `local.rs:91-94` — `.expect()` on `reqwest::Client`
  builder, which can fail if TLS roots can't be loaded. Propagate.

### 3.10 Context rebuilding inefficiency

`controller.rs:609, 635-639, 987-1003` calls `session.build_context()`
multiple times per turn and clones the full message vector. This is
fine at current scale but will show up when sessions grow into
thousands of turns. Push a filtered query API into `anie-session`.

---

## 4. Recommended refactors

Ordered by payoff-per-effort. Each is scoped to stay under the
"phased execution, ≤5 files per phase" rule.

### Refactor A — Extract TUI widgets and overlay trait (high payoff)

1. Create `crates/anie-tui/src/widgets/text_field.rs`, move `TextField`
   out of onboarding and providers, re-export from `widgets/mod.rs`.
2. Create `crates/anie-tui/src/widgets/panel.rs` containing
   `render_status_panel`, `render_busy_panel`, `centered_rect`,
   `footer_line`.
3. Define `trait OverlayScreen { fn handle_key; fn render; fn
   handle_tick; fn handle_worker_event; }`.
4. Implement `OverlayScreen` for `OnboardingScreen` and
   `ProviderManagementScreen`.
5. Replace `enum OverlayState` in `app.rs` with `Option<Box<dyn
   OverlayScreen>>`.

Payoff: deletes ~150 LOC of duplication, lets a future `/settings`
screen plug in without touching `app.rs`, and lets the overlays be
tested independently.

### Refactor B — Split `openai.rs` by responsibility

1. `crates/anie-providers-builtin/src/openai/streaming.rs` —
   `OpenAiStreamState`, `OpenAiToolCallState`, `has_meaningful_content`.
2. `crates/anie-providers-builtin/src/openai/tagged_reasoning.rs` —
   `TaggedReasoningSplitter` and its tests.
3. `crates/anie-providers-builtin/src/openai/convert.rs` —
   `assistant_message_to_openai_llm_message`, `convert_messages`,
   `convert_tools`.
4. Keep `openai.rs` (or rename `openai/mod.rs`) as the `Provider` impl
   and HTTP wiring only.
5. Add unit tests for `tagged_reasoning` (UTF-8 boundary, interleaved
   tags, unterminated tags) and `streaming` (reasoning-only stream →
   error, reasoning+text → success, tool-call reassembly across
   chunks).

Payoff: reduces `openai.rs` from 2084 LOC to ~800 LOC, unblocks
Phase 1 of the reasoning fix plan with confidence, and gives you
fast-feedback tests on the trickiest code in the crate.

### Refactor C — Split `ControllerState` and introduce a command registry

1. Extract `ModelCatalog` (resolution + caching) from `ControllerState`.
2. Extract `CompactionStrategy::run(&self, session, registry, opts)`
   that subsumes `maybe_auto_compact`, `force_compact`, and
   `retry_after_overflow`.
3. Extract `RetryPolicy` from the main event loop.
4. Introduce `trait SlashCommand` and register handlers in a
   `CommandRegistry`; derive help text from handler metadata.
5. Make `ProviderRegistry` accessible via a trait boundary that
   `anie-session` depends on, or pass a closure instead.

Payoff: `controller.rs` shrinks from 1967 LOC, each concern becomes
unit-testable, the three mode paths (interactive / print / RPC) stop
fighting for the same mutable state.

### Refactor D — Unify provider request building and model discovery

1. Create `crates/anie-providers-builtin/src/request.rs` with a
   small `HttpRequestBuilder` that handles headers, auth style,
   timeout, retry, and status classification.
2. Collapse `discover_openai_compatible_models`,
   `discover_anthropic_models`, and `discover_ollama_tags` into one
   `discover(api_kind, endpoint, auth) -> Vec<Model>` that dispatches
   internally.
3. Move the shared tool-call reassembly shape into
   `streaming::ToolCallAssembler` with the same signature for both
   providers.
4. Share the `reqwest::Client` across discovery + streaming; don't
   build a new one per request.

Payoff: deletes another ~200 LOC of duplication and removes the
"these two providers drift" failure mode.

### Refactor E — Tighten the error taxonomy

1. Audit every `ProviderError::Stream(_)` / `::Other(_)` / `::Request(_)`
   call site, write down what each actually represents.
2. Add concrete variants
   (`InvalidJson`, `EmptyAssistantResponse`, `ToolCallMalformed`,
   `Timeout`) and migrate the call sites.
3. Replace string `.contains("…")` retry checks with enum matches.

Payoff: small file count, eliminates the "string-typed error API"
anti-pattern.

### Refactor F — Housekeeping

1. Delete `anie-extensions` or populate the `Extension` trait and
   wire `hooks.rs` to actually call into it.
2. Add `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo fmt --all -- --check` as separate CI jobs.
3. Add `fd-lock` to `anie-session` or document and log the
   single-writer assumption.
4. Cache the `ToolRegistry` on `ControllerState`; stop rebuilding it
   per run.
5. Consolidate `.anie/` path construction into one helper in
   `anie-config`.

---

## 5. Prioritization

If you're picking one thing this week: **Refactor B** (split
`openai.rs` and add streaming tests). It is the highest-leverage
change — it unblocks the reasoning-fix plan you've already written,
it covers the code that is statistically most likely to break on
provider changes, and it keeps to a single crate.

If you have a second slot: **Refactor A** (TUI overlay trait +
shared widgets). It deletes the most duplication in one move, and
`ideas.md` makes it clear you're about to add more overlay screens.

Do **Refactor F.2** (CI enforcement) today — it's ten minutes of YAML
and it protects everything else.

Leave `anie-extensions` as-is only if you can name, in one sentence,
what the contract will be. Otherwise delete it this week.

---

## 6. What's deliberately out of scope for this review

- The ideas backlog in `docs/ideas.md` — that's a roadmap, not a code
  review finding.
- `docs/reasoning_fix_plan.md` — already a sound plan; this review
  only notes that Refactor B makes Phase 1 of it cheaper.
- Security and sandboxing — the README already flags "tools currently
  run without sandboxing or approvals." That's a known v1 trade-off.
- Performance micro-benchmarks — the code is fine for current
  workloads and the ideas doc already has a benchmarking item.
