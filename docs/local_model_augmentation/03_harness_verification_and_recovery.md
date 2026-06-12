# Plan 03 — Harness-side verification + failure recovery

## 1. Rationale

Small models are bad at noticing their own mistakes; the
harness's deterministic checks aren't. Today every mitigation
in this area is **read-side** — the harness advises, the model
must act:

- The system-prompt re-test rule
  (`docs/harness_mitigations_2026-05-01/03_system_prompt_retest.md`)
  tells the model to re-run verification after edits; the plan
  doc itself flags it as a low-confidence signal under context
  pressure.
- `wrap_failed_tool_results` prepends "you MUST re-verify"
  directives to failed results
  (`crates/anie-agent/src/agent_loop.rs:2252`; rlm-gated
  at `crates/anie-cli/src/controller.rs:2816-2820`).
- The failure-loop detector counts consecutive identical
  failures (`(tool_name, args_hash)`, threshold 3,
  `crates/anie-agent/src/failure_loop.rs:35,71`) but is
  **observability-only** — a log line and a SystemMessage; it
  never changes what happens next.
- `VerifierPolicy` injects a one-shot *critique prompt* when
  the todo list completes (`crates/anie-cli/src/verifier.rs`,
  env-gated `ANIE_VERIFIER=1`) — it asks the model to
  self-review; it runs nothing.
- **There is no mechanism for the harness itself to execute a
  command** (verified absence): `bash` is a model-initiated
  tool only (`crates/anie-tools/src/bash.rs`).

Two more write-side gaps, found while surveying:

- **Temperature is not forwarded to Ollama at all.** The
  request body carries only `num_ctx`
  (`crates/anie-providers-builtin/src/ollama_chat/convert.rs:12-27`);
  `StreamOptions.temperature`
  (`crates/anie-provider/src/options.rs:33`) exists but the
  agent loop hardcodes `None` (`agent_loop.rs:621`). A stuck
  model resamples the same wrong tokens — the retry comment on
  `ModelOutputMalformed` (`crates/anie-provider/src/error.rs:117`)
  already relies on sampling variance that Ollama's defaults
  may not provide.
- An edit-tool match failure tells the model to re-read the
  file (the wrap directive) — costing a turn — instead of just
  *showing* it the current content.

This plan converts advice into action: run the verify command,
perturb sampling on loops, attach the file on edit failures.

## 2. Design

### 2a. Harness-run verification (`[verify]` config)

New config section (`anie-config`):

```toml
[verify]
command = "cargo check --workspace"  # no default — opt-in
timeout_s = 120                       # default 120
max_output_lines = 40                 # default 40
```

New `VerifyRunner` + `VerifyPolicy` in
`crates/anie-cli/src/verify_runner.rs`, composed into the
existing `build_agent` policy vec (rlm → verifier → budget →
…; `ChainedBeforeModelPolicy`, `agent_loop.rs:427-477`):

- **Trigger**: at `before_model`, if the working context
  contains ≥1 *successful* `edit`/`write`/`apply_patch` result
  newer than the last verify run. Debounced: at most one run
  per model turn, and skipped when the mutating call is the
  most recent message only after tool-result batching settles
  (i.e. fires on the turn *after* edits land — the same
  position `VerifierPolicy` occupies).
- **Execution**: spawned with the bash tool's process hygiene
  — own process group, `kill_on_drop`, SIGKILL on timeout
  (`bash.rs:84-130`), and the same optional sandbox spec.
- **Result injection**: `AppendMessages` with one context-only
  message (the `VerifierPolicy` precedent — present in the
  request, not persisted to the session):

```
<system-reminder source="verify">
`cargo check --workspace` FAILED (exit 101) after your edits.
First errors (40-line cap):
…head of stderr…
Fix these before proceeding; the harness re-runs this check
after your next edit.
</system-reminder>
```

  On success a single line (`verify passed: cargo check…`) so
  the model can claim completion with evidence. Output is
  hard-capped (`max_output_lines`, first-N — compile errors
  front-load), never the raw wall of text (principle 2).
- **Gating**: only when `[verify].command` is configured;
  active in rlm mode by default, `ANIE_DISABLE_VERIFY=1` to
  kill (controller gating pattern).

anie-specific note: this deliberately does **not** reuse the
`bash` tool's `Tool` interface — the harness is the caller,
not the model, and the result must not look like a model
action in the transcript.

### 2b. Failure-loop escalation (temperature perturb)

1. **Plumb temperature**: forward
   `options.temperature` into the Ollama body's `options`
   object in `build_request_body` (`convert.rs:12-27`) when
   `Some` — one-line, independently useful.
2. **Escalate on detection**: when the detector fires
   (`observe()` returning `Some(strikes)`,
   `failure_loop.rs:71`), the loop sets a per-run
   `perturbation: Option<f32>` that the next model request
   adds to temperature (default bump `+0.4`, clamped to
   `1.0`), and appends a harness note alongside the existing
   SystemMessage:

```
[harness note] The last 3 attempts called `edit` with
identical arguments and identical failures. Re-read the
target with `read`, then take a DIFFERENT approach — do not
repeat the same call.
```

3. **Reset**: any successful tool call clears the
   perturbation (back to `None` → provider default).
- Gating: `ANIE_LOOP_PERTURB=0` disables the temperature bump
  (the note still fires); detector's existing
  `ANIE_DISABLE_LOOP_DETECTOR` master switch is unchanged.

### 2c. Grounded edit-failure recovery

When `edit` or `apply_patch` fails on a text-match error and
the failure-loop detector shows a prior failure for the same
file (strikes ≥ 1), the harness appends to the failed
`ToolResult` (after the existing wrap directive) the file's
current content around the best fuzzy-match region — found
with the whitespace-insensitive matcher the edit tool already
runs (`crates/anie-tools/src/edit.rs:145-151`) — capped at 80
lines with `path:line` markers:

```
[harness note] Current content of src/foo.rs:120-180 (the
closest match to your oldText):
…
Adjust oldText to match these exact bytes.
```

This saves the read-turn the wrap directive currently asks
for. Implemented in the edit/apply_patch failure path behind
the same rlm gate as `wrap_failed_tool_results` (the two
compose: directive first, grounding second).

## 3. Files to touch

- `crates/anie-config/src/…` (`[verify]` section)
- `crates/anie-cli/src/verify_runner.rs` (new)
- `crates/anie-cli/src/controller.rs` (policy composition,
  gating)
- `crates/anie-providers-builtin/src/ollama_chat/convert.rs`
  (temperature forwarding)
- `crates/anie-agent/src/agent_loop.rs` (perturbation state,
  detector escalation, edit-failure grounding hook)
- `crates/anie-agent/src/failure_loop.rs` (expose per-file
  strike lookup)
- `crates/anie-tools/src/edit.rs` / `apply_patch.rs` (surface
  best-match region in the typed error for the grounding hook)
- `crates/anie-cli/src/run_metrics.rs` +
  `crates/anie-evals/` (PR 4)

## 4. Phased PRs

**PR 1 — `local_aug/PR9: [verify] config + harness-run verification policy`**

**PR 2 — `local_aug/PR10: forward temperature to Ollama + loop-perturbation`**
Two commits: the plumbing refactor, then the escalation
(separable-refactor rule).

**PR 3 — `local_aug/PR11: grounded edit-failure recovery`**

**PR 4 — `local_aug/PR12: recovery metrics + broken-fixture eval scenarios`**
`RunMetrics.recovery { verify_runs, verify_failures,
loop_perturbations, grounded_edit_failures }` (schema bump —
coordinate with plan 01 PR 4, land sequentially); scenarios:
a fixture with a deliberately broken build (`expect`: final
text contains "verify passed"), and a loop-trap fixture.

## 5. Test plan

PR 1:
- `verify_runs_once_after_a_turn_containing_successful_edits`
- `verify_does_not_run_when_no_mutating_tool_succeeded`
- `verify_failure_injects_capped_first_n_lines_of_output`
- `verify_result_message_is_context_only_not_persisted`
- `verify_timeout_kills_process_group_and_reports_timeout`
- `unconfigured_verify_section_is_byte_identical_noop`

PR 2:
- `stream_options_temperature_lands_in_ollama_options_object`
- `absent_temperature_omits_the_field_entirely` (regression:
  existing request bodies unchanged)
- `third_identical_failure_bumps_next_request_temperature`
- `successful_tool_call_resets_perturbation_to_provider_default`
- `perturbation_is_clamped_at_one_point_zero`

PR 3:
- `second_match_failure_on_same_file_attaches_best_match_region`
- `first_failure_does_not_attach_content` (wrap directive
  only — don't pay the tokens until a repeat proves the model
  is stuck)
- `attachment_is_capped_at_eighty_lines_with_line_markers`
- `grounding_composes_after_the_wrap_directive`

PR 4:
- `run_metrics_reports_recovery_counters`
- `older_metrics_schema_loads_with_recovery_defaulted`
- broken-fixture scenario passes with verify on, fails the
  `contains` check with verify off (negative control), live
  qwen3:8b smoke.

## 6. Risks

- **Verify storms on chatty editors.** A model that edits
  every turn pays one verify per turn. Debounce is per-turn;
  if smoke shows pain, add an N-edits threshold knob
  (Deferred until measured).
- **Verify command is user-supplied and runs unsandboxed by
  default** — same trust level as the model-invoked bash tool;
  inherits the sandbox spec when configured. Documented in the
  config comment.
- **Temperature bump degrades good runs.** It applies only
  after 3 identical failures and resets on first success;
  negative-control scenario in PR 4 watches pass-rate.
- **Grounding attachment bloats failed results.** 80-line cap,
  fires only on the *second* identical failure, and the rlm
  failure-eviction signals (Signal A/B,
  `context_virt.rs:576-690`) already reclaim superseded
  failures.

## 7. Exit criteria

- [ ] All four PRs landed; tests + clippy green per PR.
- [ ] Live smoke: break `cargo check` in a fixture, ask
      qwen3:8b to fix it — the model receives the harness
      verify failure, fixes, and the run ends with
      "verify passed" without the user prompting a re-test.
- [ ] Live smoke: a forced edit-loop (stale oldText) resolves
      within 2 attempts after grounding, where today it loops
      to the detector threshold.
- [ ] Request-body diff confirms temperature absent unless
      perturbation or explicit config sets it.
- [ ] Hosted-provider behavior byte-identical with `[verify]`
      unset.

## 8. Deferred

- N-edits debounce threshold for verify (add when measured).
- Auto-detected verify commands (`cargo check` when Cargo.toml
  present, etc.) — explicit config first; detection invites
  surprise execution.
- Best-of-N sampling / self-consistency voting for high-stakes
  steps (needs the temperature plumbing from PR 2; own plan).
- Local-model cascade on persistent loops (8B → 32B handoff) —
  follow-up series.
- Wiring the verify result into `VerifierPolicy`'s critique
  (let the self-review cite the actual check output).

---

## 9. Amendment (2026-06-12): Signal C — near-duplicate call loops

Field evidence (session `0f9cd627`, qwen3.5:0.8b,
[field notes](field_notes/2026-06-12_qwen3.5-0.8b_session.md) F4): ten
consecutive `web_search` calls with escalating near-duplicate queries,
all "successful", none caught — the failure-loop detector requires
identical `(tool, args_hash)` AND `is_error`. The small-model loop
signature is *similar, successful, useless* calls.

### Design

Extend `FailureLoopDetector` (or a sibling `SimilarCallDetector` in
`failure_loop.rs` — decide at PR time, same module either way):

- Track the last K=5 calls per tool. For string-bearing argument
  values, compute a token set (lowercased, alphanumeric split — reuse
  the tokenizer convention from `context_virt`'s keyword overlap).
- A new call whose token-set Jaccard overlap with any of the last K
  same-tool calls exceeds 0.6 increments a similarity streak
  (regardless of is_error); a dissimilar call or different tool resets
  it.
- At streak 3: emit the existing-style `SystemMessage` +
  `tracing::info`, AND inject a harness note into the next tool
  result: "your last 3 `web_search` calls were near-duplicates; the
  archive ledger lists what they returned — answer from those results
  or take a different action. Do NOT search again for the same thing."
- At streak 5: arm the plan-03 PR10 temperature perturbation (shared
  machinery — Signal C is a second producer for the same per-run
  `perturbation` slot).
- Gate: on in rlm mode, `ANIE_DISABLE_SIMILAR_CALL_DETECTOR=1` off;
  threshold env-tunable like the existing detector.

### PR

**PR 18 — `local_aug/PR18: near-duplicate call detector (Signal C)`**
Lands after PR 10 (shares the perturbation slot). Tests:
`near_duplicate_streak_fires_note_at_three`,
`successful_but_similar_calls_count_toward_the_streak`,
`dissimilar_call_resets_the_streak`,
`streak_of_five_arms_temperature_perturbation`,
`identical_args_still_route_through_the_existing_failure_detector`.

### Risks

- False positives on legitimately-similar calls (paging through search
  results). The note is advisory (never blocks); threshold 0.6/streak 3
  chosen high; eval scenario with legitimate pagination as a negative
  control.
