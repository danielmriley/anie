# 00 — Consolidated findings

Date: 2026-04-26
Method: five parallel Explore agents reviewed CLI, provider
crates, TUI (non-markdown), session/agent/auth/config, and
markdown layout.

Findings are tagged severity (HIGH / MEDIUM / LOW) and risk
(low / medium / high) for the implementation work to address.

## CLI / controller (`anie-cli`)

**F-CLI-1** *(MEDIUM, low risk)* — Cancel + status-event
pattern repeats ~12 sites in `controller.rs:344-587`. Each
arm calls `cancel_pending_retry_for_run_affecting_change()`
+ `send_event(status_event())`. A
`fn cancel_and_notify_status(&mut self)` helper would
collapse to one call per arm.

**F-CLI-2** *(MEDIUM, low risk)* — Persistence-warning
plumbing: `if let Some(warning) = persistence_warning {
self.send_system_message(&warning).await; }` repeats 6× in
`controller.rs`. Helper:
`fn send_persistence_warning_if_present(&self, warning:
Option<String>)`.

**F-CLI-3** *(LOW, low risk)* — Single-line wrapper functions
on `ControllerState`:
- `controller.rs:1087-1089` `session_diff` (1 caller)
- `controller.rs:1091-1093` `session_context` (1 caller)
- `controller.rs:1095-1097` `context_without_entry` (1 caller)
- `controller.rs:1138-1140` `list_sessions` (1 caller)
- `controller.rs:777-779` `current_model_uses_ollama_chat_api`
  (2 callers — keep, has shape value)

Inline the 1-caller wrappers.

**F-CLI-4** *(MEDIUM, medium risk)* — Test fixture
duplication: 3 controller-builder variants in
`controller_tests.rs:195-486` plus 2 context-length-specific
builders. Each repeats `tempdir + AnieConfig::default() +
build_tool_registry + SystemPromptCache + SessionManager +
ConfigState + ControllerState` (~40 LOC × 5 sites =
~200 LOC). A `ControllerTestBuilder` with chainable
`.with_*()` would consolidate.

**F-CLI-5** *(MEDIUM, low risk)* — Print/RPC/interactive
mode initialization duplication:
`print_mode.rs:18-21`, `rpc.rs:15-18`,
`interactive_mode.rs:16+41`. All call
`prepare_controller_state` + create channels +
`InteractiveController::new`. Extract
`spawn_controller_from_cli(cli, exit_after_run)` in
`bootstrap.rs`.

**F-CLI-6** *(LOW, low risk)* — `apply_config_change`
parametric helper could collapse SetModel / SetResolvedModel /
SetThinking arms (each ~15 LOC, identical guard / apply /
notify shape).

## Provider crates

**F-PROV-1** *(HIGH, high risk)* — SSE streaming state
machine duplication. OpenAI / Anthropic / Ollama each
re-implement nearly-identical event-reassembly logic:
- `openai/streaming.rs` — 816 LOC
- `anthropic.rs` — ~600 LOC of stream-state code
- `ollama_chat/streaming.rs` — parallel implementation

Empty-assistant guards, tool-call buffering, usage
accumulation, thinking-block handling — invariant across all
three. ~400 LOC consolidatable via shared `SseStateMachine`
trait. **High risk** because correctness regressions affect
all providers.

**F-PROV-2** *(MEDIUM, low risk)* — Error classification
chain: `util.rs:12-39 classify_http_error` is correctly
shared. The provider-specific 400-pre-check pattern repeats
in `anthropic.rs:56-68` and `openai/reasoning_strategy.rs:174-189`.
Cosmetic improvement, not a correctness issue.
`parse_retry_after()` is called 11× identically.

**F-PROV-3** *(LOW, low risk)* — Provider init boilerplate.
`OpenAIProvider::new`, `AnthropicProvider::new`,
`OllamaChatProvider::new` are all identical 8-line shape:
```rust
pub fn new() -> Self {
    Self { client: shared_http_client().cloned()
        .unwrap_or_else(|_| crate::http::create_http_client()) }
}
```
~30 LOC across 3 providers. Macro or helper could collapse.

**F-PROV-4** *(LOW, low risk)* — Reasoning-family lists in
two places:
- `local.rs:78` — `REASONING_FAMILIES`
- `model_discovery.rs` — inline list in `infer_reasoning()`

Single source-of-truth function would prevent drift.

**F-PROV-5** *(HIGH, very high risk)* — OAuth provider
duplication across five providers:
- `anthropic_oauth.rs` 387 LOC
- `openai_codex_oauth.rs` 451 LOC
- `github_copilot_oauth.rs` 635 LOC
- `google_antigravity_oauth.rs` 607 LOC
- `google_gemini_cli_oauth.rs` 887 LOC

~3,000 LOC total, ~70% boilerplate (PKCE, callback server,
token refresh, store/retrieve). A generic
`AuthCodeFlowProvider` template could save ~1,700 LOC.
**Very high risk**: each provider has subtle endpoint quirks;
abstraction must not leak. Defer until next OAuth provider
addition forces the design.

**F-PROV-6** *(LOW, low risk)* — Sample-model test fixtures
duplicated across provider tests. ~13 variants. Centralize
in `anie-provider/src/tests.rs` shared module.

## TUI rendering (non-markdown)

**F-TUI-1** *(HIGH, medium risk)* — Three header builders
share a near-identical pattern:
- `output.rs:1602` `format_tool_header_spans` (bullet + verb
  + args)
- `output.rs:1314` `assistant_thinking_lines` (bullet + label)
- `output.rs:1281` `assistant_error_lines` (bullet + label)

A `build_bullet_header(bullet, label, args?, style?) ->
Vec<Span>` helper would collapse ~25 LOC.

**F-TUI-2** *(HIGH, medium risk)* — Overlay frame boilerplate
in three overlays:
- `overlays/model_picker.rs:113-124`
- `overlays/providers.rs:229-238`
- `overlays/onboarding.rs:317-326`

Each builds the same `centered_rect + Block::default() +
Borders::ALL + BorderType::Rounded + cyan-bold title + DIM
border + Clear`. ~30 LOC × 3 = ~90 LOC. Helper:
`fn render_overlay_frame(area, title, body_fn)`.

**F-TUI-3** *(MEDIUM, medium risk)* — Two spinner systems
coexist:
- Original `Spinner` braille cycle (`app.rs:324`) — used in
  tool block headers, thinking sections, and overlay
  loading states.
- New `breathing_bullet` (`app.rs:2210`) — only used in
  `render_spinner_row`.

The braille spinner is still consumed in 4+ sites. Decide:
retire braille and switch all sites to breathing, or keep
both with documented purpose split.

**F-TUI-4** *(LOW, low risk)* — `display_path` in
`app.rs:2106` is a one-line wrapper called twice. Inline.

**F-TUI-5** *(LOW, low risk)* — `thinking_gutter_style` and
`thinking_body_style` in `output.rs:1398-1410` are single-
line helpers each called once. Inline.

**F-TUI-6** *(MEDIUM, low risk)* — `ToolCallResult` re-export
chain: defined in `output.rs:49`, re-exported via
`app.rs:52` then again via `lib.rs:17`. Direct re-export
from output to lib eliminates the chain.

## Session / agent / auth / config

**F-CONFIG-1** *(HIGH, low risk)* — Path helper proliferation
in `anie-config/src/lib.rs:430-458`:
- `global_config_path` `→ ~/.anie/config.toml`
- `anie_auth_json_path` `→ ~/.anie/auth.json`
- `anie_sessions_dir` `→ ~/.anie/sessions`
- `anie_logs_dir` `→ ~/.anie/logs`
- `anie_state_json_path` `→ ~/.anie/state.json`

All five are `anie_dir().map(|d| d.join(...))` with a
constant subpath. ~40 LOC consolidatable via a single helper:
```rust
fn anie_subpath(name: &str) -> Option<PathBuf>
```
Or — keep the named accessors but have them delegate.
Ergonomic compromise: callers prefer named accessors; impl
shares one line.

**F-CONFIG-2** *(MEDIUM, low risk)* — Atomic-write parent-dir
inconsistency. Four `atomic_write` call sites:
- `anie-config/src/mutation.rs:119-121` ← creates parent first ✓
- `anie-config/src/lib.rs:539-541` ← creates parent first ✓
- `anie-auth/src/lib.rs:388` ← does NOT create parent ✗
- `anie-auth/src/store.rs:442` ← does NOT create parent ✗

Risk: if parent is missing, atomic_write returns
`InvalidInput`. Fix: wrap atomic_write in
`atomic_write_create_parent(path, bytes)` that always
ensures the parent exists.

**F-CONFIG-3** *(MEDIUM, low risk)* — Deprecated public
wrapper `anie-auth/src/lib.rs:431-432`:
```rust
#[deprecated]
pub fn auth_file_path() -> Option<PathBuf> {
    default_auth_file_path()
}
```
Has no callers per grep. Safe to remove.

**F-AGENT-1** *(LOW, low risk)* — `AgentLoopConfig` builder
methods (`agent_loop.rs:255-317`) all set one field each.
Idiomatic builder; no consolidation.

**F-SESSION-1** *(LOW)* — Compaction structure clean. No
findings.

## Markdown layout deep dive

**F-MD-1** *(HIGH, medium risk)* — Table rendering: anie's
multi-pass column negotiation (`layout.rs:662-980`) vs. pi's
single-pass token walk (`packages/tui/src/components/markdown.ts:679-850`).

Anie:
- `compute_column_widths` (66 LOC) — proportional shrink loop
- `wrap_table_row` (18 LOC) — transpose-then-wrap
- `wrap_plain_text_cell` (50+ LOC) — custom word-break
- `pad_cell` (23 LOC)
- `table_data_row` + `table_border_line` (~40 LOC)
- 10 tests pinning glyph-level details

Pi: ~170 LOC total in `markdown.ts`. Walks tokens, emits lines
inline, single fallback check.

Adopting pi's approach saves 80–100 LOC. Risk: relaxing
glyph-pinned tests, accepting simpler-but-coarser cell wrap.

**F-MD-2** *(MEDIUM, medium risk)* — List state machine
(`layout.rs:65-70`, `:538-576`). `pending_first_line_prefix`
is a one-shot override that adds a side-channel state.
pulldown-cmark already gives depth via event nesting.
Inline bullet-marker construction at `Start(Item)` and drop
the override field. ~40-50 LOC.

**F-MD-3** *(LOW)* — `push_blank_separator` (24 LOC) called
3 times. Acceptable extraction; not a primary target.

**F-MD-4** *(MEDIUM, low risk)* — Tests pin implementation
details (`table_renders_with_unicode_box_drawing` and
~9 others). Relax to behavioral assertions before
attempting F-MD-1.

## Mapping findings to plans

| Finding | Plan |
|---------|------|
| F-CONFIG-1, F-CONFIG-2, F-CONFIG-3 | 01 (safe wins) |
| F-CLI-3 (single-line inlines), F-PROV-4 (reasoning families), F-TUI-4, F-TUI-5, F-TUI-6 | 01 (safe wins) |
| F-CLI-1, F-CLI-2, F-CLI-5 | 01 (safe wins) — small enough |
| F-CLI-4 (test builder) | 01 (safe wins) — mechanical |
| F-CLI-6 (parametric apply) | 02 — needs more thought |
| F-TUI-1, F-TUI-2, F-TUI-3 | 02 (TUI render) |
| F-MD-1, F-MD-2, F-MD-4 | 03 (markdown) |
| F-PROV-1 | 04 (SSE) |
| F-PROV-5 | 05 (OAuth) |

PR 01 (safe wins) lands now on this branch. Plans 02–05 stay
as docs for the user to read in the morning.
