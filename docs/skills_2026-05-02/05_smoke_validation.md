# PR 5 — Smoke validation of the skills system

## Rationale

PRs 1-4 ship the skills system end-to-end. PR 5 measures
whether it actually changes behavior. The 2026-05-01 +
2026-05-02 smoke runs documented several failure modes
that the bundled skills target directly; PR 5's job is
to re-run the smoke and check whether the model loads
the relevant skills autonomously, and whether loading
them changes the failure-mode rates.

## Scenario

The 11-turn smoke protocol from
`docs/smoke_protocol_2026-05-01.md`, run against
qwen3.5:9b, in `--harness-mode=rlm`, with the four
bundled skills installed. Compare against the
post-mitigations baseline (commits up through the
harness_mitigations PR 3 follow-up at `495a6bb`).

## What to measure per turn

For each of the eleven turns, log:

1. **Wall-clock duration** (compared to post-mitigations
   baseline).
2. **rlm: ledger line** (evicted N, paged in M, archive: K).
3. **Tool calls issued** — particularly `skill` calls.
4. **Skills loaded** — the controller logs `skill loaded`
   at info level; grep the run log for the skill names.
5. **`[loop warning]` outcomes** — should be no different
   than baseline; skills shouldn't change the loop
   detector's behavior.
6. **`[tool error]` recovery quality** — did the model
   load `verify-after-edit` after a failed bash call?

## Targeted hypotheses

Each bundled skill has a specific turn it should help on:

| Turn | Bundled skill | Expected behavior |
|---|---|---|
| T1 — implement DLL | `cpp-rule-of-five` should load if the model writes any class with `new` / `delete` | The DLL implementation should NOT regress to `~Class() = default;` next to raw `new`. If the skill doesn't load, the failure mode from 2026-05-02 likely repeats. |
| T2 — driver + compile | `decompose-multi-constraint-task` should load if the model rewrites the same file 3+ times | Should reduce the cycling-through-same-bug pattern observed in baseline T2. |
| T5 — apply improvements | `verify-after-edit` should load after the edit completes | Should stop the T5 → T7 chain where the bug shipped at T5 only surfaced at T7. |
| T7 — recompile + run reverse() | `verify-after-edit` already loaded from T5; if not, should load here | Failure mode resolution from PR 1 (failed-result wrap) should be reinforced. |
| T10 — wardrobe pivot | `use-recurse-for-archive-lookup` should NOT load (no archive content relevant) | Model should reach for web tools directly. Confirms the model isn't loading skills cargo-cult. |
| T11 — explicit weather | `use-recurse-for-archive-lookup` should NOT load if T11 is fresh; if the conversation was long, it might | Same. |

A skill that loads autonomously when expected = signal
the system is working. A skill that doesn't load when
expected = signal we need either (a) better skill
descriptions or (b) more explicit prompt-augment
guidance. A skill that loads when NOT expected = signal
the agent is over-loading; tighten descriptions.

## Comparison metrics

Update the table in
`docs/smoke_protocol_2026-05-01.md` with three new rows:

```
| Skills loaded autonomously (count) | n/a | ?  | |
| T2 cycling rewrites (count)        | ~6  | ?  | |
| T5-introduced bug surfaces at T5 (vs. T7) | T7 | ? | |
```

Plus the existing rows from
`smoke_protocol_2026-05-01.md`'s scoring table.

## Procedure

1. Build the binary: `cargo build` (debug is fine).
2. Verify bundled skills load:
   `./target/debug/anie --print "/skills"` — should list
   the four bundled skills.
3. Fresh session, 11 turns per
   `docs/smoke_protocol_2026-05-01.md`.
4. After each turn, dump the run's tracing log and
   grep for "skill loaded" entries.
5. After T11, run `/skills` to capture the final
   active-in-this-run set.
6. Update the smoke protocol comparison table.
7. If a skill loaded incorrectly or didn't load when
   expected, document the diagnosis (description too
   broad? too narrow? prompt-augment issue?).

## Files to touch

- `docs/smoke_protocol_2026-05-01.md` — extend the
  scoring table with skills-related rows and add a
  paragraph in the "What to score per run" section
  about logging skill loads.

No code changes — this is a measurement PR.

## Test plan

Not applicable — measurement, not implementation.

## Risks

- **No autonomous loading.** The model sees the
  catalog but never invokes `skill`. Mitigation:
  reinforce in the rlm augment (PR 2 already did
  one line; could expand). If still no, the next
  iteration explores auto-loading via harness
  detection (deferred from PR 2's docs).
- **Over-loading.** The model loads skills that don't
  apply, polluting context. Mitigation: tighten the
  `description` field on bundled skills.
- **Smoke takes too long to iterate.** The 11-turn
  scenario can be 30-60+ minutes against qwen3.5:9b.
  Mitigation: a shorter "smoke-lite" variant (3-5
  turns) for fast iteration on skill descriptions.
  Defer until needed.

## Exit criteria

- [ ] Smoke run completed against `dev_rlm` HEAD with
      the bundled skills loaded.
- [ ] Updated comparison table in
      `smoke_protocol_2026-05-01.md` reflects skill
      loading rates and behavioral deltas.
- [ ] At least one bundled skill loads autonomously
      during the smoke (otherwise the system isn't
      doing its job and the next iteration looks at
      surfacing).
- [ ] No regressions on the existing scoring rows.

## Deferred

- Auto-loading via harness pattern detection
  (e.g., the harness sees `new`/`delete` in an edit
  and pre-loads `cpp-rule-of-five`). Tempting but
  fragile; defer until skills-by-discovery is
  measurably insufficient.
- A "skills" benchmark suite (run skills + recurse
  + decompose against SWE-bench-lite) — proper eval
  work, separate plan series.
