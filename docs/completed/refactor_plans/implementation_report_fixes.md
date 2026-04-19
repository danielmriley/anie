# Implementation report — refactor fixes

Implemented the fixes in `docs/refactor_plans/fixes/` in order and reviewed the result after each fix-plan phase with targeted diffs/tests.

## 01 — Colocate OpenAI submodule tests
- Moved conversion tests into `crates/anie-providers-builtin/src/openai/convert.rs`.
- Moved stream-state tests into `crates/anie-providers-builtin/src/openai/streaming.rs`.
- Moved reasoning-strategy tests into `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`.
- Left only provider/integration-shaped tests in `crates/anie-providers-builtin/src/openai/mod.rs`.
- Added the top-level OpenAI module layout doc comment.
- Result: `openai/mod.rs` dropped from 1449 LOC to 836 LOC with production code unchanged.

## 02a — Overlay placeholder stubs
- Added `render_placeholder_panel(...)` in `crates/anie-tui/src/widgets/panel.rs` and re-exported it from `widgets/mod.rs`.
- Added `OverlayOutcome::Dismiss` and `OverlayOutcome::Idle`, and handled both in `App::apply_overlay_outcome`.
- Added six placeholder overlays:
  - `session_picker.rs`
  - `settings.rs`
  - `oauth.rs`
  - `theme_picker.rs`
  - `hotkeys.rs`
  - `tree.rs`
- Updated `crates/anie-tui/src/overlays/mod.rs` to declare the stubs and fix the module docs.
- Added render/dismiss/tick tests for every stub plus helper coverage for `render_placeholder_panel`.

## 02b — Finish clone audit + typed provider key
- Removed render-time `OnboardingState` cloning by matching `&self.state` in `onboarding.rs`.
- Removed render-time `ProviderManagementMode` cloning by matching `&self.mode` in `providers.rs`.
- Added `OnboardingState::Transient` plus take/restore helpers so the `return_to` transitions stop cloning state.
- Converted provider-management `test_results` from `HashMap<String, TestResult>` to `HashMap<usize, TestResult>`.
- Added clearing logic when the provider row set changes.
- Added onboarding/provider tests covering the non-mutating render path, transient restoration, row-indexed test results, reload clearing, and row-deletion cleanup.

## 03a — Slash-command dispatch / help wiring
- Added registry-backed `CommandRegistry::format_help()` in `crates/anie-cli/src/commands.rs`.
- Wired `/help` through `UiAction::ShowHelp` instead of rendering a hard-coded TUI-local help block.
- Updated the TUI slash-command parser to emit `UiAction::ShowHelp`.
- Updated the controller to emit the formatted registry help as a `SystemMessage`.
- Added registry/help tests, including extension-group rendering and registry coverage for every dispatched slash command.
- Trimmed the dead-code scaffolding in `commands.rs` by making the remaining test-only registration surface `#[cfg(test)]`.

## 03b — RetryPolicy extraction + taxonomy reconciliation
- Added `RetryDecision`, `GiveUpReason`, and `RetryPolicy::decide()` in `crates/anie-cli/src/retry_policy.rs`.
- Migrated controller retry behavior to the new policy and removed the old `should_retry_*` helpers.
- Switched transient retry scheduling to accept a precomputed delay.
- Removed `ProviderError::is_retryable()` from `crates/anie-provider/src/error.rs`.
- Kept `retry_after_ms()` as the trivial accessor for server-provided retry hints.
- Added retry-policy unit tests plus controller-level retry-path tests for:
  - compaction retry success
  - second-overflow give-up
  - transient retry exhaustion
- Updated `docs/refactor_plans/05_provider_error_taxonomy.md` to reflect the final retry shape.

## 03c — Finish the controller split
- Added `crates/anie-cli/src/runtime/config_state.rs` and moved config/runtime selection logic into `ConfigState`.
- Split controller startup/runtime support into new modules/files:
  - `crates/anie-cli/src/bootstrap.rs`
  - `crates/anie-cli/src/interactive_mode.rs`
  - `crates/anie-cli/src/print_mode.rs`
  - `crates/anie-cli/src/rpc.rs`
  - `crates/anie-cli/src/controller_tests.rs`
- Moved tests out of `controller.rs` and into:
  - `controller_tests.rs`
  - `model_catalog.rs`
  - `retry_policy.rs`
  - `runtime/prompt_cache.rs`
  - `runtime/config_state.rs`
- Moved `build_agent` to a free function.
- Added a struct-level `ControllerState` layout doc comment.
- Result: `crates/anie-cli/src/controller.rs` is now 921 LOC.

## 06/07 — Architecture doc refresh
- Removed stale extension-crate references from:
  - `docs/arch/anie-rs_architecture.md`
  - `docs/arch/anie-rs_build_doc.md`
- Added the session advisory-lock note to `anie-rs_architecture.md`.
- Added plan-10 pointers for the future extension design.
- Updated `docs/refactor_plans/README.md` with a note that the doc refresh landed.
- Updated `docs/refactor_plans/implementation_review_2026-04-18.md` to mark the doc gaps fixed.

## 07 — Hook visibility narrowing
- Narrowed hook types in `crates/anie-agent/src/hooks.rs` to `pub(crate)`.
- Removed the public re-exports from `crates/anie-agent/src/lib.rs`.
- Reworked `AgentLoopConfig` construction so hooks are internal-only (`with_hooks(...)`) and no longer appear in the public config surface.
- Updated all `AgentLoopConfig` call sites across the workspace.
- Added/updated hook-related tests so the internal hook path remains covered.

## 08 — Status hygiene + missing send_event tests
- Updated the status header in `docs/refactor_plans/08_small_hygiene_items.md` to reflect that phases B and D already landed.
- Added direct `send_event` tests in `crates/anie-agent/src/agent_loop.rs` for:
  - warn-once on closed channel
  - no warning on successful send
  - process-global latch behavior across channels
- Updated `docs/refactor_plans/implementation_review_2026-04-18.md` to mark the fix complete.

## Verification
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

## Spot checks
- `crates/anie-providers-builtin/src/openai/mod.rs` = 836 LOC
- `crates/anie-cli/src/controller.rs` = 921 LOC
- `rg 'BeforeToolCallHook|AfterToolCallHook|BeforeToolCallResult|ToolResultOverride' crates | grep -v '^crates/anie-agent/'` → no hits
- `rg 'anie-extensions|anie_extensions' docs/arch README.md` → no hits
