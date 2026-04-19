# GPT-5.4 Prompt — Implement Integration Tests Phase by Phase

Use the following prompt with GPT-5.4.

---

You are implementing integration tests for the `anie` project inside its repository.

Your job is to implement the integration test suite **phase by phase, in order**, following the testing phase documents exactly.

## Primary objective

Create a dedicated integration test crate (`anie-integration-tests`) and implement 14 integration tests across 4 categories, verifying the cross-crate wiring that unit tests do not cover.

Work through the phases in order. Do not skip ahead. Do not move to the next phase until the current phase's exit criteria are satisfied.

## Read these docs first

Before writing any code, read these documents completely:

1. `@docs/integration_testing_plan.md` — the overall rationale and test design
2. `@docs/testing_phases/README.md` — phase overview
3. `@docs/testing_phases/phase_0_test_crate_and_infrastructure.md`
4. `@docs/testing_phases/phase_1_agent_tools_session.md`
5. `@docs/testing_phases/phase_2_session_resume.md`
6. `@docs/testing_phases/phase_3_agent_tui.md`
7. `@docs/testing_phases/phase_4_config_wiring.md`

Also read enough of the existing codebase to understand the APIs you will call:
- `crates/anie-agent/src/lib.rs` and `crates/anie-agent/src/agent_loop.rs` — `AgentLoop`, `AgentLoopConfig`, `AgentRunResult`, `ToolRegistry`, `ToolExecutionMode`
- `crates/anie-agent/src/tests.rs` — existing agent test patterns and helpers
- `crates/anie-provider/src/mock.rs` — `MockProvider`, `MockStreamScript`
- `crates/anie-provider/src/provider.rs` — `Provider` trait, `ProviderEvent`
- `crates/anie-provider/src/options.rs` — `StreamOptions`, `ResolvedRequestOptions`, `RequestOptionsResolver`
- `crates/anie-protocol/src/events.rs` — `AgentEvent`
- `crates/anie-protocol/src/content.rs` — `ContentBlock`
- `crates/anie-protocol/src/messages.rs` — `Message`, `AssistantMessage`, `UserMessage`
- `crates/anie-tools/src/lib.rs` — `ReadTool`, `WriteTool`, `EditTool`, `BashTool`, `FileMutationQueue`
- `crates/anie-session/src/lib.rs` — `SessionManager`, `SessionContext`, `SessionEntry`, `SessionEntryBase`
- `crates/anie-tui/src/app.rs` — `App`, `App::new`, `App::handle_agent_event`, `App::render`
- `crates/anie-tui/src/tests.rs` — existing TUI test patterns and `render_to_string`
- `crates/anie-config/src/lib.rs` — `load_config_with_paths`, `configured_models`, `CliOverrides`
- `crates/anie-auth/src/lib.rs` — `AuthResolver`
- `crates/anie-providers-builtin/src/lib.rs` — `register_builtin_providers`, `builtin_models`

## Source-of-truth precedence

If the phase docs and the existing code differ on an API signature, **the existing code wins**. Adjust the test implementation to match the real API. Do not modify production code.

If you discover a real bug in production code while writing tests, note it as a comment in the test but do not fix it. Continue implementing the test suite.

## Non-negotiable constraints

1. **Do not modify production code.** These are pure test additions.
2. **Do not add new workspace dependencies.** Use only crates already in `Cargo.toml`.
3. **Use `MockProvider` for all provider interactions.** No real network calls.
4. **Use `tempfile::TempDir` for all file-system tests.** No writes outside temp directories.
5. **All tests must be `#[tokio::test]` where async is needed.**
6. **Run `cargo fmt --all` after each phase.**

## How to execute

### Phase 0 — Test crate and shared infrastructure

Read: `@docs/testing_phases/phase_0_test_crate_and_infrastructure.md`

Do:
1. Create `crates/anie-integration-tests/Cargo.toml` with the dependencies listed in the phase doc.
2. Create `crates/anie-integration-tests/src/lib.rs` exporting a `helpers` module.
3. Create `crates/anie-integration-tests/src/helpers.rs` with all 8 helper functions described in the phase doc.
4. Add `"crates/anie-integration-tests"` to the workspace `members` list in the root `Cargo.toml`. Do NOT add it to `default-members`.
5. Run `cargo check -p anie-integration-tests`.
6. Run `cargo test -p anie-integration-tests` (should show 0 tests, no errors).
7. Run `cargo test --workspace` to confirm existing tests still pass.

Gate: all three commands succeed. Do not proceed until this is green.

### Phase 1 — Agent loop → real tools → session persistence

Read: `@docs/testing_phases/phase_1_agent_tools_session.md`

Do:
1. Create `crates/anie-integration-tests/tests/agent_session.rs`.
2. Implement all 5 test cases described in the phase doc.
3. Each test must:
   - create a temp directory
   - seed any needed files
   - create a `MockProvider` with scripted responses
   - create a real tool registry with `real_tool_registry(cwd)`
   - run the agent loop via `run_agent_collecting_events`
   - persist the user prompt and generated messages to a `SessionManager`
   - reopen the session and verify `build_context()` returns the expected messages
4. Run `cargo test -p anie-integration-tests`.
5. Run `cargo fmt --all`.

Gate: all 5 tests pass.

### Phase 2 — Session resume and context continuity

Read: `@docs/testing_phases/phase_2_session_resume.md`

Do:
1. Create `crates/anie-integration-tests/tests/session_resume.rs`.
2. Implement all 3 test cases described in the phase doc.
3. Test 6: persist a conversation, reopen, rebuild context, run a new agent loop with the rebuilt context.
4. Test 7: persist an assistant message with `ContentBlock::Thinking`, reopen, verify thinking blocks survive.
5. Test 8: persist messages, add a compaction entry, reopen, verify the compacted context drives a new agent run.
6. Run `cargo test -p anie-integration-tests`.
7. Run `cargo fmt --all`.

Gate: all 3 tests pass (cumulative 8 tests).

### Phase 3 — Agent events → TUI rendering

Read: `@docs/testing_phases/phase_3_agent_tui.md`

Do:
1. Create `crates/anie-integration-tests/tests/agent_tui.rs`.
2. Implement all 3 test cases described in the phase doc.
3. Each test must:
   - run the agent loop and collect `AgentEvent`s
   - replay the events into a TUI `App` via `handle_agent_event`
   - render to a `TestBackend`
   - extract the screen text and assert on expected content
4. For the render-to-string helper, follow the pattern from `crates/anie-tui/src/tests.rs` (`render_to_string`).
5. Use a terminal size of at least `80x24`.
6. Run `cargo test -p anie-integration-tests`.
7. Run `cargo fmt --all`.

Gate: all 3 tests pass (cumulative 11 tests).

### Phase 4 — Config → provider registry wiring

Read: `@docs/testing_phases/phase_4_config_wiring.md`

Do:
1. Create `crates/anie-integration-tests/tests/config_wiring.rs`.
2. Implement all 3 test cases described in the phase doc.
3. Test 12: verify `register_builtin_providers` populates OpenAI and Anthropic API kinds.
4. Test 13: write a TOML config to a temp file, load it, call `configured_models`, verify the model entry.
5. Test 14: set a test env var, create an `AuthResolver` with the config, resolve, verify the key. Clean up the env var.
6. Run `cargo test -p anie-integration-tests`.
7. Run `cargo fmt --all`.

Gate: all 3 tests pass (cumulative 14 tests).

### Final validation

After all phases are complete:
1. Run `cargo test --workspace` and report the full result.
2. Run `cargo fmt --all` one final time.
3. Report: total test count across the workspace, any warnings, any failures.

## Handling problems

### If a helper function signature doesn't match the real API

Adjust the helper to match the real API. The phase docs describe the intended shape, but the existing code is the source of truth.

### If a test case is ambiguous

Make the smallest reasonable interpretation that exercises the cross-crate boundary described in the phase doc. Add a comment explaining your choice.

### If a test fails due to a likely production bug

Add a `// NOTE: possible production bug — [description]` comment. If the bug is minor, use `#[ignore]` with a reason string. If the bug blocks the entire test file, stop and report.

### If the mock provider runs out of scripts

You probably scripted the wrong number of provider responses. Count the tool-call rounds: each round consumes one script. The final answer consumes one more.

## Deliverable style

For each phase, report:
- which phase you completed
- files created or modified
- test count and pass/fail status
- whether the next phase is unblocked

After the final phase, report:
- total workspace test count
- any warnings or issues discovered
- confirmation that `cargo test --workspace` is green

## Start

Begin by reading the docs listed above, then start with **Phase 0**.
