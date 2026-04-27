# Implementation roadmap — code-review remediation + active input + mid-turn compaction

This roadmap coordinates the three April 27 plan sets:

- `docs/code_review_2026-04-27/` — hardening and correctness follow-ups
  from the comprehensive code review.
- `docs/active_input_2026-04-27/` — editable input, queued follow-ups,
  and interrupt-and-send while the agent is running.
- `docs/midturn_compaction_2026-04-27/` — mid-turn compaction, context-
  aware reserve sizing, adaptive tool output caps, and compaction
  telemetry. Targets the small-context local-model use case where a
  single user turn can fan out into enough sampling requests to
  overrun the configured context window.

The goal is to land these changes in reviewable increments without
mixing mechanical formatting, security hardening, UX/controller
semantics, and context-handling infrastructure in the same commits.

## Roadmap principles

1. **CI first.** The repository currently fails `cargo fmt --all --
   --check`; fix that before behavior changes so every later PR has a
   clean validation baseline.
2. **Keep active-input work honest.** anie remains a single-run agent.
   Follow-up prompts are drafted/queued and run at safe boundaries; they
   are not injected into an active provider stream.
3. **Security boundary before web polish.** The web SSRF/redirect fix is
   the highest-risk code-review item and should land before treating web
   tools as stable.
4. **Cancellation before timeouts; config before policy.** Web tools must
   honor cancellation. New wall-clock limits should be centralized and
   configurable, not scattered as hardcoded stops.
5. **Small PRs, one behavior per PR.** Each PR should have a focused
   test list and exit criteria. Avoid combining format-only, config,
   controller queue semantics, and network boundary changes.
6. **Preserve session order.** Queued prompts should persist only when
   they actually start, after the current run has finished or aborted.
7. **Local models are first-class.** The mid-turn compaction work
   targets workstation-class hosts running Ollama. Defaults must keep
   small-context models usable end-to-end without manual tuning, and
   no change in this roadmap may regress the cloud-model pre-prompt
   compaction behavior.

## Dependency overview

```text
Format baseline
  ├─ [ui] config loading
  │    └─ configurable web budgets
  ├─ active input: editable draft → queue → interrupt/send
  ├─ web boundary: manual redirects → DNS/private-IP validation → headless policy
  │    └─ robots/Defuddle correctness
  ├─ web cancellation → bounded side channels → configurable budgets
  ├─ streaming read cap
  ├─ Ollama num_ctx message fix
  ├─ atomic-write durability clarification
  └─ mid-turn compaction: context-aware reserve → per-turn budget → agent-loop hook
       → mid-turn execution → context-aware tool output caps → telemetry
```

## Phase 0 — Clean baseline

### PR 0.1 — Format-only CI unblocker

**Plan:** `docs/code_review_2026-04-27/01_repo_formatting_ci.md`

**Why first:** `cargo fmt --all -- --check` currently fails and CI has a
fmt job.

**Scope:**

- Run `cargo fmt --all`.
- Commit only rustfmt output.

**Validation:**

- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

**Exit gate:** all later PRs branch from a clean format baseline.

---

## Phase 1 — Quick user-visible correctness wins

These are small, high-confidence fixes that unblock later work and give
immediate value.

### PR 1.1 — Load `[ui]` from real config files

**Plan:** `docs/code_review_2026-04-27/02_ui_config_loading.md`

**Scope:**

- Add `PartialUiConfig`.
- Merge `[ui]` values in `load_config_with_paths()`.
- Add real-loader tests.
- Optionally document `[ui]` in the default config template.

**Why early:**

- Fixes a real user-facing bug.
- Provides the config merge pattern needed for `[tools.web]` budgets in
  Phase 4.

**Validation:**

- `cargo test -p anie-config ui`
- workspace tests/clippy.

### PR 1.2 — Editable draft while agent runs

**Plan:** `docs/active_input_2026-04-27/01_editable_active_draft.md`

**Scope:**

- Route active-state printable/editing keys into `InputPane`.
- Keep Ctrl+C/Ctrl+D active behavior unchanged.
- Guard Enter while active so drafts are not lost before queueing lands.
- Adjust active input styling/docs so the box no longer communicates
  "frozen".

**Why early:** solves the minimum UX ask without controller/session queue
semantics.

**Validation:**

- Active typing tests in `anie-tui`.
- Existing Ctrl+C active tests remain green.

### PR 1.3 — Use effective Ollama `num_ctx` in load-failure message

**Plan:** `docs/code_review_2026-04-27/06_ollama_effective_num_ctx_message.md`

**Scope:**

- Pass `effective_ollama_context_window()` into rich
  `ModelLoadResources` messaging.
- Add override regression test.

**Why early:** independent, small correctness fix.

**Validation:**

- `cargo test -p anie-cli user_error`
- relevant controller tests.

---

## Phase 2 — Safe follow-up queue semantics

This phase turns active drafting into actual queued follow-ups while
preserving the single-run invariant.

### PR 2.1 — TUI queued-prompt action

**Plan:** `docs/active_input_2026-04-27/02_queued_followups.md` PR A

**Scope:**

- Add `UiAction::QueuePrompt(String)`.
- Active Enter sends `QueuePrompt` for ordinary non-slash drafts.
- Empty active Enter remains no-op.
- Slash-command behavior stays explicit and guarded.

**Validation:**

- TUI tests proving active Enter sends `QueuePrompt` and clears draft
  only after queue action is emitted.

### PR 2.2 — Controller FIFO prompt queue

**Plan:** `docs/active_input_2026-04-27/02_queued_followups.md` PR B

**Scope:**

- Add `queued_prompts: VecDeque<String>` to `InteractiveController`.
- Handle `QueuePrompt` while active/idle/pending retry.
- Start queued prompts after current run is persisted.
- Preserve print-mode exit semantics.

**Validation:**

- FIFO queue tests.
- Session ordering tests: current generated messages before queued user
  prompt.

### PR 2.3 — Queue vs retry/backoff policy

**Plan:** `docs/active_input_2026-04-27/02_queued_followups.md` PR C

**Scope:**

- Queued prompts suppress stale transient retries.
- Queueing during `PendingRetry::Armed` clears the retry and starts or
  queues the user prompt.

**Validation:**

- Retry/queue interaction tests.
- Existing retry cancellation tests remain green.

### PR 2.4 — Queue visibility polish

**Plan:** `docs/active_input_2026-04-27/02_queued_followups.md` PR D

**Scope:**

- Emit visible system messages when a prompt is queued and when a queued
  prompt starts.
- Keep status-bar queue count deferred unless cheap.

**Validation:**

- Controller event tests for queued/start messages.
- Manual smoke of queued follow-up.

---

## Phase 3 — Web network security boundary

This phase addresses the highest-risk finding before deeper web-tool UX
or extraction polish.

### PR 3.1 — Manual redirect handling

**Plan:** `docs/code_review_2026-04-27/03_web_ssrf_redirect_boundary.md` PR A

**Scope:**

- Disable reqwest automatic redirects for web fetches.
- Implement manual redirect loop.
- Validate every redirect target before sending the next request.

**Validation:**

- Existing redirect tests updated/passing.
- New regression proving redirect-to-loopback is rejected before the
  loopback endpoint observes a request.

### PR 3.2 — DNS/resolved-IP private-address validation

**Plan:** `docs/code_review_2026-04-27/03_web_ssrf_redirect_boundary.md` PR B

**Scope:**

- Add resolver abstraction or connector integration.
- Reject hostnames resolving to private/link-local/loopback/etc. IPs
  when `allow_private_ips == false`.
- Extend IP classification for IPv4-mapped IPv6 and missing special
  ranges as needed.

**Validation:**

- Deterministic resolver tests for public and private mappings.
- `allow_private_ips = true` bypass test.

### PR 3.3 — Headless path policy

**Plan:** `docs/code_review_2026-04-27/03_web_ssrf_redirect_boundary.md` PR C

**Scope:** choose safe near-term shape:

- Conservative: keep headless feature-gated and add an explicit config
  or documentation gate saying it is not SSRF-equivalent to the
  non-headless path; or
- Stronger: implement Chrome request interception for private
  destinations.

**Validation:**

- Headless feature compile check.
- Tests for whatever guard/policy lands.

---

## Phase 4 — Web cancellation, bounded memory, configurable budgets

This phase should respect the long-running-agent policy: cancellation and
memory caps are mandatory; wall-clock budgets are centralized and
operator-tunable.

### PR 4.1 — Web-tool cancellation

**Plan:** `docs/code_review_2026-04-27/04_web_cancellation_budgets.md` PR A

**Scope:**

- Thread `CancellationToken` through `web_read` and `web_search`.
- Select against cancellation during rate-limit waits, fetches, headless
  render, and Defuddle.
- Kill children on cancellation.

**No new short timeout policy in this PR.**

**Validation:**

- Fake-runner/fake-backend cancellation tests.
- Manual abort smoke for long web read.

### PR 4.2 — Bound web side-channel reads

**Plan:** `docs/code_review_2026-04-27/04_web_cancellation_budgets.md` PR B

**Scope:**

- Bound non-2xx error bodies.
- Bound `robots.txt` bodies.
- Bound Defuddle stdout/stderr.
- Return typed/truncated errors.

**Validation:**

- Huge error-body fixture.
- Huge robots fixture.
- Defuddle stderr flood fake.

### PR 4.3 — Configurable `[tools.web]` budgets

**Plan:** `docs/code_review_2026-04-27/04_web_cancellation_budgets.md` PR C

**Depends on:** Phase 1.1 `[ui]` config merge pattern.

**Scope:**

- Add minimal `[tools.web]` config.
- Move web defaults into config/options structs.
- Keep persistent-agent-friendly defaults.
- Support explicit long/disabled wall-clock budgets only with clear
  validation/docs.

**Validation:**

- Config absent → old defaults.
- Config present → values reach web tools.
- Invalid values produce clear config-load errors.

### PR 4.4 — Optional progress updates

**Plan:** `docs/code_review_2026-04-27/04_web_cancellation_budgets.md` PR D

**Scope:**

- Coarse `ToolExecUpdate` phase messages: fetching, rendering,
  extracting.
- Do not emit high-frequency progress.

**Validation:**

- Tool-update forwarding tests.

---

## Phase 5 — Resource hardening outside web fetches

### PR 5.1 — Image metadata pre-check in `read`

**Plan:** `docs/code_review_2026-04-27/05_streaming_read_cap.md` PR A

**Scope:**

- Check image file size with metadata before reading into memory.

**Validation:**

- Oversized image rejected without full read.
- Small image behavior unchanged.

### PR 5.2 — Streaming text `read`

**Plan:** `docs/code_review_2026-04-27/05_streaming_read_cap.md` PR B

**Scope:**

- Stream/chunk text files.
- Stop once output cap/line cap/requested limit is satisfied.
- Preserve UTF-8 safety and binary detection.

**Validation:**

- Large file with small `limit` returns bounded output.
- Offset/limit/truncation tests.

### PR 5.3 — Read footer/details cleanup

**Plan:** `docs/code_review_2026-04-27/05_streaming_read_cap.md` PR C

**Scope:**

- Adjust remaining-lines footer semantics where exact counts would
  require scanning the whole file.
- Keep details payload useful/backward-compatible.

**Validation:**

- Updated read-output tests.

---

## Phase 6 — Web standards/extraction polish and durability cleanup

### PR 6.1 — robots.txt user-agent and origin correctness

**Plan:** `docs/code_review_2026-04-27/07_robots_and_defuddle_correctness.md` PR A

**Scope:**

- Cache robots by origin, not just host.
- Evaluate with configured user-agent.

**Validation:**

- `User-agent: anie` vs wildcard tests.
- Same host/different port cache test.

### PR 6.2 — Defuddle source URL handling

**Plan:** `docs/code_review_2026-04-27/07_robots_and_defuddle_correctness.md` PR B

**Scope:**

- If Defuddle supports a base/source URL flag, pass it.
- Otherwise document the limitation and optionally post-process relative
  Markdown links safely.

**Validation:**

- Command-builder or post-processing tests, depending on final design.

### PR 6.3 — Atomic-write durability clarification

**Plan:** `docs/code_review_2026-04-27/08_atomic_write_durability.md`

**Scope:**

- Either fsync parent directory after rename on Unix, or clarify docs if
  implementation is deferred.

**Validation:**

- `cargo test -p anie-config atomic_write`

---

## Phase 7 — Interrupt-and-send UX

This phase can land earlier if product urgency is high, but it is safest
after queued prompts are stable.

### PR 7.1 — Controller abort-and-queue action

**Plan:** `docs/active_input_2026-04-27/03_interrupt_and_send.md` PR A

**Scope:**

- Add explicit abort-and-queue action.
- Active run → front-queue prompt and cancel current run.
- Pending retry → clear retry and start prompt.

**Validation:**

- Controller ordering/cancellation tests.

### PR 7.2 — TUI affordance

**Plan:** `docs/active_input_2026-04-27/03_interrupt_and_send.md` PR B

**Scope:**

- Add chosen keybinding or command fallback.
- Empty draft must not abort unexpectedly.
- Ctrl+C remains unchanged.

**Validation:**

- TUI action tests.
- Manual terminal smoke, especially if using `Ctrl+Enter`.

### PR 7.3 — Discoverability

**Plan:** `docs/active_input_2026-04-27/03_interrupt_and_send.md` PR C

**Scope:**

- Update help/status text to describe active typing, queued Enter, and
  interrupt/send shortcut.

**Validation:**

- Help formatting tests if present.

---

## Phase 8 — Mid-turn compaction and small-context handling

This phase ports codex's mid-turn compaction pattern to anie and
adds the small-context-aware sizing the cloud-targeted reference
implementations don't bother with. It is **independent of Phases
1–7** in implementation; the only loose dependency is on Phase 1.3
(Ollama `num_ctx` message), since this phase formalizes the
"effective" reserve and tool-output budgets that surface in user-
facing error and `/state` text.

The full plan set lives at
`docs/midturn_compaction_2026-04-27/`. Detailed PR-level scope
and tests are in the per-plan files; the phase entries here track
ordering and gates only.

### PR 8.1 — Context-aware compaction reserve

**Plan:** `docs/midturn_compaction_2026-04-27/01_context_aware_reserve.md` PR A

**Scope:**

- `effective_reserve(window, configured, min_reserve)` helper.
- `Controller::compaction_strategy` uses it.
- Optional `min_reserve_tokens` config knob (PR B, follow-up).

**Why first:** unblocks both 8.2 (budget assumes reasonable
threshold) and 8.4 (mid-turn execution uses the effective value).

**Validation:**

- Property test on `effective_reserve` over a range of windows.
- Existing compaction integration tests still pass for large
  windows.

### PR 8.2 — Per-turn compaction budget (counter + reactive)

**Plan:** `docs/midturn_compaction_2026-04-27/02_per_turn_compaction_budget.md` PRs A + B

**Scope:**

- Add `compactions_this_turn: u32` counter on the controller.
- Add `[compaction] max_per_turn` (default 2).
- Reactive overflow path consults the budget;
  `GiveUpReason::CompactionBudgetExhausted` for exhaustion.

**Why early:** anti-thrash protection lands before mid-turn so
the moment 8.4 is on, runaway compaction storms are already
bounded.

**Validation:**

- `retry_policy_gives_up_when_budget_exhausted`.
- End-to-end test injecting consecutive `ContextOverflow`s.

### PR 8.3 — Agent-loop compaction signal

**Plan:** `docs/midturn_compaction_2026-04-27/03_agent_loop_compaction_signal.md`

**Scope:**

- `CompactionGate` trait + `CompactionGateOutcome` enum.
- `AgentConfig::compaction_gate: Option<Arc<dyn CompactionGate>>`.
- Hook fires at the top of each post-first agent-loop iteration.
- `build_agent` passes `None` for now; behavior unchanged.

**Why early:** pure plumbing, default-off, lets 8.4 land as a
focused behavior change against a stable hook.

**Validation:**

- `agent_run_with_no_gate_behaves_like_today`.
- `agent_run_calls_compaction_gate_between_iterations`.

### PR 8.4 — Mid-turn compaction execution

**Plan:** `docs/midturn_compaction_2026-04-27/04_midturn_compaction_execution.md` PRs A + B

**Scope:**

- Refactor `Session::compact_internal` into a pure
  `compact_messages_inline` helper.
- `ControllerCompactionGate` installed via `build_agent`.
- Mid-turn compactions emit `CompactionStart` / `CompactionEnd`.
- `TranscriptReplace` fires after a successful mid-turn
  compaction so the TUI re-renders.

**Validation:**

- `midturn_compaction_fires_when_context_exceeds_threshold`.
- `midturn_compaction_does_not_fire_under_threshold`.
- Tool-call correlation preservation test.
- Cancellation-during-compaction test.
- Manual smoke against a small-context Ollama model.

### PR 8.5 — Mid-turn path consults the budget

**Plan:** `docs/midturn_compaction_2026-04-27/02_per_turn_compaction_budget.md` PR C

**Scope:**

- The mid-turn gate handler checks the budget before initiating
  compaction; when exhausted, returns
  `CompactionGateOutcome::Skipped` with a reason.

**Why here:** must follow 8.4 because the gate doesn't exist
until then.

**Validation:**

- `midturn_compaction_skipped_when_budget_exhausted`.

### PR 8.6 — Tool output caps scale with context

**Plan:** `docs/midturn_compaction_2026-04-27/05_tool_output_caps_scale_with_context.md`

**Scope:**

- Plumb `context_window` into `ToolExecutionContext`.
- `effective_tool_output_budget(window, base_default)` helper.
- Apply to `bash`, built-in `read`, and `web_read`.
- `[tools] context_share_for_output` config knob (default 0.1).

**Why now:** independent of 8.3/8.4 in code but complements them.
Land after 8.4 so the activity row labels (8.7) capture both
mid-turn compactions and any tool-cap-driven shrinkage uniformly.

**Validation:**

- Cloud-vs-local regression test.
- `bash_truncates_stdout_to_effective_budget_for_small_window`.

### PR 8.7 — Compaction telemetry and visibility

**Plan:** `docs/midturn_compaction_2026-04-27/06_compaction_telemetry.md`

**Scope:**

- Add `CompactionPhase` enum on `CompactionStart` /
  `CompactionEnd` events.
- `CompactionStats` per session in the controller.
- TUI activity row distinguishes pre-prompt / mid-turn /
  reactive labels.
- `/state` summary shows the counts.
- Optional `/compaction-stats` slash command.

**Why last:** lands against the now-stable mid-turn machinery
so the labels and counts reflect real behavior.

**Validation:**

- Forward-compat session-log load test.
- `compaction_stats_increments_pre_prompt_counter`,
  `..._mid_turn_counter`, `..._reactive_overflow_counter`.

---

## Parallelization opportunities

After Phase 0, these workstreams can proceed mostly independently:

- **Active input track:** Phases 1.2, 2, and 7.
- **Web safety track:** Phases 3 and 4.
- **Small correctness track:** Phase 1.3, Phase 5, Phase 6.3.
- **Context handling track:** Phase 8 (8.1 → 8.2 → 8.3 → 8.4 → 8.5 → 8.6 → 8.7).
  Independent of all other phases at the source-file level except for
  shared edits to `crates/anie-cli/src/controller.rs` (see below).

Avoid parallel edits to the same files:

- `crates/anie-tui/src/app.rs` is touched by active input, may also
  need help/discoverability updates, and gets the per-phase activity-
  row labels in Phase 8.7. Sequence the active-input PRs before 8.7.
- `crates/anie-cli/src/controller.rs` is touched by queue semantics,
  interrupt/send, the Ollama message fix, and every Phase 8 PR (8.1
  effective reserve, 8.2 budget counter, 8.4 gate installation, 8.7
  stats). Land the small Ollama fix and the queue refactors before
  the Phase 8 controller changes if possible — Phase 8's controller
  edits are concentrated around the compaction call sites, but a
  careful merge is required if the queue work is in flight.
- `crates/anie-config/src/lib.rs` is touched by `[ui]`, `[tools.web]`,
  streaming read config only if added, atomic-write docs, and Phase 8's
  `min_reserve_tokens`, `max_per_turn`, and `context_share_for_output`
  knobs. Sequence config PRs carefully; the Phase 8 knobs do not
  conflict with the other phases at the field level.
- `crates/anie-tools-web/src/read/fetch.rs` is touched by SSRF,
  cancellation/bounds, robots correctness, and Phase 8.6's effective
  byte budget. Prefer the order: manual redirects → DNS validation →
  side-channel caps → robots → effective byte budget.
- `crates/anie-session/src/lib.rs` is touched by Phase 8.4's refactor
  of `compact_internal` into `compact_messages_inline`. No conflict
  with other phases.
- `crates/anie-agent/src/agent_loop.rs` is touched by Phase 8.3's
  `CompactionGate` hook and Phase 8.6's `context_window` plumbing.
  Land 8.3 before 8.6 so the agent-loop edits stack cleanly.

## Validation gates

### Per PR

Run unless the PR is docs-only:

- `cargo fmt --all -- --check`
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`

### Web PRs

- `cargo test -p anie-tools-web`
- `cargo check -p anie-tools-web --features headless`
- `cargo check -p anie-cli --features web-headless`
- Manual public `web_search` → `web_read` smoke.
- Manual abort smoke for long `web_read` after Phase 4.1.

### Active-input PRs

- `cargo test -p anie-tui active`
- `cargo test -p anie-cli queue` once queue work begins.
- Manual smoke:
  - type while streaming;
  - Enter queues after Phase 2;
  - Ctrl+C still aborts;
  - interrupt-and-send after Phase 7.

### Mid-turn compaction PRs

- `cargo test -p anie-session compact`
- `cargo test -p anie-cli compaction`
- `cargo test -p anie-agent` (covers `CompactionGate` hook tests).
- Property test on `effective_reserve` over a range of windows
  (Phase 8.1).
- Compaction-storm fault-injection test (Phase 8.2 PR B).
- Forward-compat session-log load test for `CompactionPhase`
  (Phase 8.7 PR A).
- Manual smoke against a small-context Ollama model:
  - run a coding task with several large file reads;
  - observe at least one mid-turn compaction in the activity row;
  - confirm the turn completes without `ContextOverflow`;
  - confirm `/state` shows non-zero `mid_turn` count.

### Milestone end-to-end smoke

Before calling the roadmap complete:

1. Start a long-running run and type a queued follow-up while it streams.
2. Confirm queued prompt starts after the current run.
3. Start a web-backed task and abort during `web_read`; confirm prompt
   returns and no subprocess remains.
4. Fetch a public page through a redirect chain.
5. Attempt redirect-to-loopback/private-host fixture; confirm no private
   request is sent.
6. Read a large text file with `limit = 20`; confirm bounded output.
7. Set `[ui]` config values and confirm TUI startup honors them.
8. Trigger an Ollama load-resource error with an active `/context-length`
   override, or run the regression test if manual setup is unavailable.
9. Run a small-context Ollama coding task (e.g. 16K window) that
   exercises multiple tool calls; confirm at least one mid-turn
   compaction fires, the budget is respected, and the run completes
   without surfacing `ContextOverflow` to the user.
10. On a large-context cloud model, confirm the same task profile
    runs without firing mid-turn compaction (no regression for the
    non-local case).

## Definition of done

- CI is green: fmt, tests, clippy.
- Active input supports drafting, queueing, and an explicit interrupt
  path without losing drafts or violating session order.
- Web tools enforce private-destination guards on the non-headless path,
  honor cancellation, and bound memory side channels.
- Web timing/budget policy is centralized and configurable for
  persistent-agent deployments.
- Large file reads are bounded in memory.
- Mid-turn compaction fires when warranted, is bounded by a per-turn
  budget, surfaces visibly in the TUI, and is observable via
  per-session counters.
- Tool output caps scale with the configured context window so a
  single tool result never claims a disproportionate share of a small
  model's context.
- The small correctness/durability fixes are either landed or explicitly
  deferred with rationale in their execution trackers.
