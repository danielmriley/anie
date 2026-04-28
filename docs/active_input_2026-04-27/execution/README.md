# Execution tracker — active_input_2026-04-27

Status key:

- ⬜ Not started
- 🟡 In progress
- ✅ Landed
- ⏸ Deferred

| Plan | Status | Notes |
|---|---:|---|
| 01 — Editable active draft | ✅ | Landed `<TBD>` (2026-04-27). PR A only; PR B (border styling cleanup) deferred. Four tests pin the behavior. |
| 02 — Queued follow-up prompts | ✅ | Plan landed across four PRs (2.1–2.4) on 2026-04-27. Visibility messages emit on queue + drain; queue overrides stale retries. Persistence to disk explicitly deferred per plan. |
| 03 — Interrupt-and-send affordance | ✅ | PR A `cb788f6` (controller `AbortAndQueuePrompt`: front-queue + cancel for active runs; clear-retry-and-start for pending; start for idle). PR B `c827fb7` (Ctrl+Enter binding; empty-draft no-op; Ctrl+C unchanged). PR C landed (`/help` Keys section). |

## Validation checklist

After each PR:

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo test --workspace`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`

Focused checks:

- [ ] Active typing test: printable key while `AgentUiState::Streaming`
      updates `input_pane_contents()`.
- [ ] Active Ctrl+C test: first Ctrl+C still sends `UiAction::Abort`.
- [ ] Draft-loss test: Enter while active never silently clears a draft.
- [ ] Queue test: active Enter sends/executes queued follow-up after the
      current run.
- [ ] Ordering test: queued user prompt is persisted after the current
      run's generated messages.
- [ ] Interrupt test: abort-and-send cancels current run and starts the
      queued draft after abort completion.

## Manual smoke script

1. Start an agent response that streams for several seconds.
2. While it is streaming, type a new sentence in the input box.
3. Confirm the input box updates immediately and cursor movement works.
4. Press Enter:
   - after Plan 01 only: draft should remain with a non-destructive
     message;
   - after Plan 02: draft should queue and clear.
5. Confirm Ctrl+C still aborts the active run.
6. After Plan 03, type a correction and trigger interrupt-and-send.
