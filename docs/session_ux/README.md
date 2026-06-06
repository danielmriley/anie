# Session UX: picker + checkpoint/rewind

Initiative #8 from `docs/rival_analysis_2026-06-06/README.md`
(impact 3 / effort 4). This plan turns three verified gaps into
shippable PRs:

- **SESSION-1** — the session picker overlay is a non-functional
  stub (`crates/anie-tui/src/overlays/session_picker.rs:1-41`).
- **SESSION-3 / SESSION-5** — `/session` has no checkpoint/rewind;
  there is no working-tree snapshot/restore. This is the
  rival-distinguishing piece (Claude Code "rewind").
- **SESSION-4** *(optional)* — `/fork` works but never summarizes
  the abandoned branch.

**SESSION-2 (tree-overlay branch visualization) is DEFERRED** — see
§8. Its stub (`crates/anie-tui/src/overlays/tree.rs:1-40`) stays as
a placeholder.

A calibration note from the rival analysis applies throughout:
much of the substrate already exists (leaf-pointer branching,
arbitrary-entry fork, file-op tracking). This plan *exposes and
wires* that substrate; it does not rebuild it.

> **pi-evidence caveat.** The pi reference tree is not present on
> this machine (`docs/rival_analysis_2026-06-06/README.md`). Every
> pi claim below is sourced from `docs/anie_vs_pi_comparison.md`
> and is flagged as such; no live pi `file:line` is cited because
> none is available. Several "Codex does X" rival baselines in the
> findings are explicitly SPECULATIVE and are treated as
> hypotheses, not facts.

---

## 1. Rationale

### 1.1 The picker is a dead stub (SESSION-1)

`crates/anie-tui/src/overlays/session_picker.rs` is 91 LOC of
placeholder. The `SessionPickerScreen` `OverlayScreen` impl
returns `OverlayOutcome::Dismiss` on **any** key
(`session_picker.rs:30-32`), `OverlayOutcome::Idle` on tick
(`:34-35`), and renders `render_placeholder_panel` with hardcoded
text (`:38-40`). It holds **zero state** — no session list, no
cursor, no search — and is **never instantiated**: there is no
`open_session_picker()` in `app.rs` (contrast
`open_model_picker_for_current_provider`, `app.rs:1522`). Today the
only way to change sessions is the text commands `/session list`
(`controller.rs:845-848`) and `/session <id>` (`:850-877`).

We already have a polished, search-first picker to mirror:
`ModelPickerPane` (`crates/anie-tui/src/overlays/model_picker.rs:28-39`)
with `filtered_indices`, `selected`, `scroll`, fuzzy `SearchField`,
wraparound navigation (`:178-239`), and an async populate flow
(`open_model_picker_for_current_provider`, `app.rs:1522-1574` →
background discovery → `handle_worker_event`, `app.rs:1617`).
Crucially, the model picker is a **`BottomPane` variant**
(`app.rs:179-187`), *not* an `OverlayScreen`. That is the pattern
worth following; the `OverlayScreen` stub is the wrong substrate
and should be retired.

### 1.2 No mid-session undo / no working-tree rewind (SESSION-3, SESSION-5)

`SESSION_SUBCOMMANDS = ["list"]` (`commands.rs:29`). `/session`
with no argument currently emits `GetState` (`app.rs:1346-1349`);
there is no rewind, checkpoint, or branch-navigation action in the
30-variant `UiAction` enum (`app.rs:227-290`). The `/diff` view is
read-only metadata (`session_handle.rs:130-188`), and a codebase
grep finds **zero** `git reset/checkout/restore/revert` calls
(SESSION-5). A user who lets the agent make a series of edits and
then wants to undo them has no framework-level path back.

The substrate to fix this already exists and is the reason effort
is "4" not "8":

- `SessionManager::fork(from_entry_id)` (`lib.rs:604-610`) already
  re-points the active leaf at **any** prior entry — exactly the
  conversation half of a rewind. It is unused by the CLI;
  `SessionHandle` only exposes fork-from-current-leaf
  (`session_handle.rs:123-128`).
- File-op tracking already exists: `extract_compaction_details`
  (`lib.rs:287-312`) walks assistant tool calls and records
  `read`/`write`/`edit` paths into `CompactionDetails`
  (`lib.rs:256-267`). The rewind store reuses this exact
  tracked-file set — the brief calls this out explicitly.
- `sha2 = "0.10"` is already a workspace dep (`Cargo.toml:66`),
  and `tempfile`/`similar` are present. No git library is needed
  (or wanted — `git2`/`gix` would pull a large C/transitive tree
  and break in non-git working dirs).

Rival framing (per the findings, treated as hypotheses): Claude
Code's rewind is the headline differentiator; Codex/pi rewind is
**not clearly documented** for either rival. So we build the piece
that is genuinely distinguishing (working-tree restore) and keep
it small.

### 1.3 Fork drops the abandoned branch on the floor (SESSION-4)

`fork_to_child_session` (`lib.rs:613-630`) clones the active
branch into a child file with a `parent_session` link
(`SessionHeader.parent_session`, `lib.rs:108`) but writes **no**
record summarizing what was abandoned. `docs/anie_vs_pi_comparison.md:378-391`
notes pi has `branchWithSummary(branchFromId, summary)` appending a
`BranchSummaryEntry` "with file operations preserved" (pi source
not citable here). `CompactionDetails` already gives us the
file-op vector for free at fork time — it is just never
instantiated there (SESSION-4 scaffolding note). This is the
*optional* tail of the plan.

---

## 2. Design

### 2.1 Workstream A — Session picker (`BottomPane`, not overlay)

**Shape we land.** A `SessionPickerPane` mirroring `ModelPickerPane`,
held as a new `BottomPane::SessionPicker` variant
(`app.rs:179-182`). It reuses `ModelPickerPane`'s `SearchField`,
wraparound, and scroll logic verbatim in structure (we copy the
shape, not share code — the row contents differ). Replace the dead
`SessionPickerScreen` `OverlayScreen` stub in
`overlays/session_picker.rs` with the real pane; delete the
`OverlayScreen` impl (it is dead — `OverlayOutcome` never routes to
it). `tree.rs`'s stub is untouched (SESSION-2 deferred).

**Data flow (the one real design decision).** The TUI cannot read
session files directly and `anie-protocol::AgentEvent`
(`events.rs:30-121`) must **not** gain a dependency on
`anie-session` (it has none today — `anie-protocol/Cargo.toml`).
So we mirror the model-picker's request→event→populate flow:

1. `/session` (no arg) → new `UiAction::OpenSessionPicker`
   (replaces the current no-arg → `GetState` at `app.rs:1346-1349`).
   `/session list` (text) and `/session <id>` (direct switch) are
   retained unchanged for non-TUI / scripted use.
2. Controller handles `OpenSessionPicker` by calling the existing
   `self.state.session.list()` (`session_handle.rs:98-100` →
   `SessionManager::list_sessions`, `lib.rs:955-1015`) and emits a
   new `AgentEvent::SessionList { sessions }`.
3. `sessions` is `Vec<SessionSummary>` — a **new protocol-local
   struct** in `anie-protocol` (id, cwd, created, `modified_unix:
   u64`, message_count, first_message). This is a deliberate,
   documented deviation from reusing `anie_session::SessionInfo`
   (`lib.rs:413-429`): `SessionInfo` carries a `PathBuf` and
   `SystemTime` and lives in `anie-session`; a flat protocol struct
   keeps the crate-dependency direction clean. The controller maps
   `SessionInfo -> SessionSummary` at the boundary.
4. TUI receives `SessionList`, builds the `SessionPickerPane`,
   marks the current session (`self.state.session.id()` is passed
   through `StatusUpdate.session_id` already), and shows it.
5. `Enter` → existing `UiAction::SwitchSession(id)`
   (`app.rs:272`, handled `controller.rs:850-877`). `Esc` cancels.

No new background worker is needed (listing is a cheap local dir
read, unlike model discovery which hits the network).

**Files:** `anie-protocol/src/events.rs` (+`SessionSummary`,
+`SessionList`), `anie-tui/src/overlays/session_picker.rs`
(rewrite), `anie-tui/src/app.rs` (`BottomPane` variant, `/session`
dispatch, `AgentEvent::SessionList` handler, key routing),
`anie-cli/src/controller.rs` (`OpenSessionPicker` handler +
mapping).

### 2.2 Workstream B — Checkpoint / rewind (working-tree restore)

**Shape we land.** A content-addressed shadow store, sidecar to the
session JSONL, plus a `/rewind` that restores both the conversation
**and** the working tree to a chosen prior user turn.

- **Store:** new module `crates/anie-session/src/checkpoint.rs`
  exposing `WorkspaceCheckpointStore`. Blobs live under a sidecar
  dir `<sessions_dir>/<session_id>.checkpoints/` (a `blobs/`
  content-addressed dir keyed by `sha256(content)` and a
  `manifest.json`). **The session JSONL is not touched** — binary
  file content does not belong in the message log — so **no
  `CURRENT_SESSION_SCHEMA_VERSION` bump for rewind** (it stays at
  4, `lib.rs:90`). The manifest maps `entry_id ->
  BTreeMap<path, FileState>` where `FileState` is `Blob(hash)` or
  `Absent` (file did not exist at that point).
- **Tracked set:** the union of `modified_files` from
  `extract_compaction_details` (`lib.rs:287-312`, `write`/`edit`
  only) over the session branch. We snapshot exactly those paths —
  reusing the existing, tested extraction so the rewind store and
  the compaction summary agree on "what the agent touched."
  Bash-driven mutations (`mv`, `>`) are **out of scope** (see §8),
  matching the bounded tracking pi/anie already do.
- **Capture point:** the controller, at the user-turn boundary
  (when it accepts a `SubmitPrompt`, *before* dispatching to the
  agent loop). It records a manifest entry keyed by the
  freshly-appended user-message entry id, snapshotting the current
  on-disk content (or `Absent`) of **every** path ever tracked in
  the session. Content addressing means unchanged files cost one
  hash and zero bytes. We capture at turn granularity rather than
  per-edit so we stay out of the `pub(crate)`,
  plan-10-reserved tool-hook seam (`anie-agent/src/hooks.rs:1-9`,
  `agent_loop.rs:1466/1519`). This is a documented deviation from
  Claude Code's per-edit checkpointing — turn granularity is the
  natural unit for anie's session model and avoids prematurely
  exposing the hook API.
- **Restore (`/rewind`):** lists the recent user turns (id +
  first line + relative time); selecting target `E`:
  1. restores the working tree — for each tracked path, write the
     blob recorded at `manifest[E][path]` or delete the file if
     `Absent`;
  2. re-points the conversation via a new
     `SessionHandle::rewind_to(entry_id)` that calls the **already
     existing** `SessionManager::fork(entry_id)` (`lib.rs:604-610`)
     — append-only branching, `build_context` (`lib.rs:744`)
     reconstructs the rewound leaf correctly;
  3. emits `TranscriptReplace` (`events.rs:66`) so the TUI redraws
     the rewound transcript, exactly as `/fork` and
     `/switch` already do (`controller.rs:801-814`,
     `:857-870`).
- **`/checkpoint` (named, lightweight):** records a manifest entry
  at the current leaf with an optional user label, so `/rewind`
  can show "named" anchors alongside turn anchors. This is the
  small symmetric half of the feature; it adds no new persistence
  shape beyond a `label: Option<String>` on the manifest entry.
- **Errors:** restore failures route through the typed taxonomy.
  Filesystem restore errors surface as `anyhow`/`ToolError`-style
  results displayed via `SystemMessage`; no string-matching of
  error text. A rewind that would clobber files modified on disk
  since the checkpoint is refused with a typed
  `CheckpointError::WorkingTreeDrifted { path }` and a clear
  message (no silent overwrite).

**New deps:** none. `sha2` (`Cargo.toml:66`) is added to
`anie-session`'s `Cargo.toml` (already a workspace dep — reuse, do
not add a new hashing crate, per CLAUDE.md §4).

**Files:** `anie-session/src/checkpoint.rs` (new),
`anie-session/src/lib.rs` (re-export + `rewind`-adjacent helpers),
`anie-session/Cargo.toml` (+`sha2`),
`anie-cli/src/runtime/session_handle.rs` (`rewind_to`, checkpoint
capture helpers — the `sessions_dir()`/`inner()` accessors at
`:43-59` are already marked "reserved for future session-picker
work", so this consumes that reserved surface),
`anie-cli/src/controller.rs` (turn-boundary capture + `/rewind`,
`/checkpoint` handlers), `anie-tui/src/app.rs` (new `UiAction`s +
rewind picker reuse), `anie-cli/src/commands.rs` (`/rewind`,
`/checkpoint` registration, extend `SESSION_SUBCOMMANDS`).

### 2.3 Workstream C — Fork branch summarization (SESSION-4, optional)

**Shape we land.** A new persisted `SessionEntry::BranchSummary`
variant (`lib.rs:126-187`) appended when `/fork` leaves a branch.
It carries a short text summary plus the `CompactionDetails`
(`lib.rs:256-267`) of the abandoned path, computed with the
existing `extract_compaction_details` (`lib.rs:287-312`). Because
this lands on the persisted `SessionEntry` enum, it **bumps
`CURRENT_SESSION_SCHEMA_VERSION` 4 → 5** and gets a forward-compat
test (older v4 sessions load with the variant simply absent). New
fields follow the project rule: `#[serde(default)]` +
`skip_serializing_if`. The summary text itself reuses the
compaction summarizer path; if no summarizer is configured we
record the file-op `CompactionDetails` with an empty prose summary
rather than skipping (file ops are the durable value per
`anie_vs_pi_comparison.md:378-391`).

This workstream is **explicitly optional** and lands last so the
schema bump is isolated and the picker + rewind value ships
without waiting on it.

---

## 3. Files to touch

| File | Workstream | Change |
|------|-----------|--------|
| `crates/anie-protocol/src/events.rs` | A | `SessionSummary` struct; `AgentEvent::SessionList` |
| `crates/anie-tui/src/overlays/session_picker.rs` | A | Replace `OverlayScreen` stub with real `SessionPickerPane` |
| `crates/anie-tui/src/app.rs` | A,B | `BottomPane::SessionPicker`; `/session` dispatch; new `UiAction`s; `SessionList` handler; rewind picker |
| `crates/anie-cli/src/controller.rs` | A,B | `OpenSessionPicker`, `Rewind`, `Checkpoint` handlers; turn-boundary capture; `SessionInfo→SessionSummary` map |
| `crates/anie-session/src/checkpoint.rs` *(new)* | B | `WorkspaceCheckpointStore`, manifest, `FileState`, `CheckpointError` |
| `crates/anie-session/src/lib.rs` | B,C | re-export checkpoint module; `SessionEntry::BranchSummary`; schema bump (C) |
| `crates/anie-session/Cargo.toml` | B | add `sha2` (existing workspace dep) |
| `crates/anie-cli/src/runtime/session_handle.rs` | B | `rewind_to`, checkpoint capture/restore helpers |
| `crates/anie-cli/src/commands.rs` | A,B | register `/rewind`, `/checkpoint`; extend `SESSION_SUBCOMMANDS` |
| `docs/arch/anie-rs_architecture.md` | all | document picker + checkpoint store (exit-criteria) |
| `docs/ROADMAP.md` | all | mark initiative #8 landed (exit-criteria) |
| `docs/notes/commands_and_slash_menu.md` | all | flip `/session` picker + `/rewind` from "Not implemented" |

Each PR below touches ≤ 5 source files.

---

## 4. Phased PRs

Order matters: A (picker) ships independently; B depends on nothing
from A but reuses the picker widget for rewind selection, so B's UI
PR lands after A; C is last (isolated schema bump).

### PR 1 — `session-ux/1`: `SessionSummary` + `AgentEvent::SessionList` (protocol)

- **Files:** `anie-protocol/src/events.rs`.
- **Scope:** add `SessionSummary` (flat: `id`, `cwd`, `created`,
  `modified_unix: u64`, `message_count: u32`, `first_message`) and
  `AgentEvent::SessionList { sessions: Vec<SessionSummary> }`. No
  behavior wired yet.
- **Tests:** `session_summary_serializes_flat_fields`,
  `agent_event_session_list_round_trips`.
- **Exit:** compiles; `AgentEvent` still `PartialEq`/`Clone`;
  `anie-protocol` gains **no** new crate deps.

### PR 2 — `session-ux/2`: `SessionPickerPane` widget

- **Files:** `anie-tui/src/overlays/session_picker.rs` (rewrite).
- **Scope:** real pane mirroring `ModelPickerPane`
  (`model_picker.rs:28-239`): backing `Vec<SessionSummary>`,
  `filtered_indices`, `selected`, `scroll`, `SearchField`,
  current-session marker, `handle_key` returning a
  `SessionPickerAction { Continue, Selected(String), Cancelled }`.
  Delete the dead `OverlayScreen` impl.
- **Tests:** `session_picker_filters_by_id_and_first_message`,
  `session_picker_enter_returns_selected_id`,
  `session_picker_escape_returns_cancelled`,
  `session_picker_navigation_wraps_at_boundaries`,
  `session_picker_marks_current_session_row`,
  `session_picker_empty_list_renders_hint`.
- **Exit:** widget unit-tested headless via `TestBackend`; no app
  wiring yet; `tree.rs` stub untouched.

### PR 3 — `session-ux/3`: wire `/session` → picker end-to-end

- **Files:** `anie-tui/src/app.rs`, `anie-cli/src/controller.rs`,
  `anie-cli/src/commands.rs`.
- **Scope:** `BottomPane::SessionPicker`; `/session` no-arg →
  `UiAction::OpenSessionPicker`; controller lists + emits
  `SessionList`; TUI populates pane; `Enter` →
  `SwitchSession`. Retain `/session list` and `/session <id>`.
  Update `/session` help text.
- **Tests:** `open_session_picker_action_emits_session_list`
  (controller), `session_list_event_populates_bottom_pane` (tui),
  `session_picker_enter_dispatches_switch_session` (tui),
  `session_no_arg_opens_picker_not_get_state` (tui).
- **Exit:** SESSION-1 closed; `/session` opens an interactive,
  searchable picker; cancel returns to editor.

### PR 4 — `session-ux/4`: `WorkspaceCheckpointStore` (no wiring)

- **Files:** `anie-session/src/checkpoint.rs` (new),
  `anie-session/src/lib.rs` (re-export), `anie-session/Cargo.toml`
  (+`sha2`).
- **Scope:** content-addressed blob store + manifest +
  `FileState{Blob,Absent}` + typed `CheckpointError`. Pure
  fs/hashing logic, no controller/TUI involvement. `capture(entry_id,
  tracked_paths, label)` and `restore(entry_id) -> RestorePlan`.
- **Tests:**
  `checkpoint_capture_dedupes_identical_blobs_by_hash`,
  `checkpoint_restore_rewrites_modified_file_to_prior_blob`,
  `checkpoint_restore_deletes_file_absent_at_target`,
  `checkpoint_restore_refuses_when_working_tree_drifted`,
  `checkpoint_manifest_round_trips_through_disk`,
  `checkpoint_capture_records_absent_for_missing_path`.
- **Exit:** store fully unit-tested against `tempfile` dirs; JSONL
  schema unchanged (version stays 4).

### PR 5 — `session-ux/5`: turn-boundary capture + `SessionHandle::rewind_to`

- **Files:** `anie-cli/src/runtime/session_handle.rs`,
  `anie-cli/src/controller.rs`.
- **Scope:** controller captures a checkpoint at each accepted
  `SubmitPrompt` (keyed by the new user entry id) over the tracked
  `modified_files` set; `SessionHandle::rewind_to(entry_id)` calls
  `SessionManager::fork(entry_id)` (`lib.rs:604`) + applies the
  store's restore plan. Consumes the reserved `sessions_dir()` /
  `inner()` accessors (`session_handle.rs:43-59`). No new slash
  command yet (driven by a test-only entrypoint).
- **Tests:** `turn_boundary_captures_tracked_files_only`,
  `rewind_to_restores_working_tree_and_forks_leaf`,
  `rewind_to_unknown_entry_returns_typed_error`,
  `rewind_emits_transcript_replace`.
- **Exit:** rewind works programmatically; conversation half reuses
  existing fork; SESSION-5 mechanism proven.

### PR 6 — `session-ux/6`: `/rewind` + `/checkpoint` UX

- **Files:** `anie-tui/src/app.rs`, `anie-cli/src/controller.rs`,
  `anie-cli/src/commands.rs`.
- **Scope:** register `/rewind` and `/checkpoint`; `/rewind` opens
  the picker widget (PR 2) populated with recent user turns +
  named checkpoints; selection drives `rewind_to`. `/checkpoint
  [name]` records a labeled anchor. Refuse rewind while a run is
  active (mirror `controller.rs:794-797`).
- **Tests:** `rewind_command_lists_user_turns_and_named_checkpoints`,
  `checkpoint_command_records_named_anchor`,
  `rewind_refused_during_active_run`,
  `rewind_selection_dispatches_rewind_to`.
- **Exit:** SESSION-3 closed; user-facing rewind/checkpoint ship.

### PR 7 — `session-ux/7`: docs + roadmap (no code)

- **Files:** `docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md`,
  `docs/notes/commands_and_slash_menu.md`.
- **Scope:** document the picker data-flow and the checkpoint store
  layout; flip the relevant "Not implemented" rows; mark initiative
  #8 (picker + rewind) landed.
- **Exit:** arch + roadmap reflect shipped state.

### PR 8 — `session-ux/8` *(optional)*: fork branch summarization (SESSION-4)

- **Files:** `anie-session/src/lib.rs` (+`SessionEntry::BranchSummary`,
  schema bump 4→5), `anie-cli/src/controller.rs` (record on fork).
- **Scope:** append a `BranchSummary` carrying the abandoned
  branch's `CompactionDetails` (+optional prose) at `/fork`.
  `#[serde(default)]` + `skip_serializing_if`; bump
  `CURRENT_SESSION_SCHEMA_VERSION`; changelog comment at
  `lib.rs:78-90`.
- **Tests:** `fork_appends_branch_summary_with_file_ops`,
  `branch_summary_defaults_when_no_summarizer`,
  `v4_session_loads_without_branch_summary` (forward-compat).
- **Exit:** SESSION-4 closed; schema migration documented.

---

## 5. Test plan

Behavior-named, per the project convention.

**Workstream A (picker)**
- `session_summary_serializes_flat_fields`
- `agent_event_session_list_round_trips`
- `session_picker_filters_by_id_and_first_message`
- `session_picker_enter_returns_selected_id`
- `session_picker_escape_returns_cancelled`
- `session_picker_navigation_wraps_at_boundaries`
- `session_picker_marks_current_session_row`
- `session_picker_empty_list_renders_hint`
- `open_session_picker_action_emits_session_list`
- `session_list_event_populates_bottom_pane`
- `session_picker_enter_dispatches_switch_session`
- `session_no_arg_opens_picker_not_get_state`

**Workstream B (checkpoint/rewind)**
- `checkpoint_capture_dedupes_identical_blobs_by_hash`
- `checkpoint_restore_rewrites_modified_file_to_prior_blob`
- `checkpoint_restore_deletes_file_absent_at_target`
- `checkpoint_restore_refuses_when_working_tree_drifted`
- `checkpoint_manifest_round_trips_through_disk`
- `checkpoint_capture_records_absent_for_missing_path`
- `turn_boundary_captures_tracked_files_only`
- `rewind_to_restores_working_tree_and_forks_leaf`
- `rewind_to_unknown_entry_returns_typed_error`
- `rewind_emits_transcript_replace`
- `rewind_command_lists_user_turns_and_named_checkpoints`
- `checkpoint_command_records_named_anchor`
- `rewind_refused_during_active_run`
- `rewind_selection_dispatches_rewind_to`

**Workstream C (optional)**
- `fork_appends_branch_summary_with_file_ops`
- `branch_summary_defaults_when_no_summarizer`
- `v4_session_loads_without_branch_summary`

Tests live in the crate closest to the logic: store tests in
`anie-session`, controller-flow tests in `anie-cli`, widget tests
in `anie-tui` (`TestBackend`, mirroring `model_picker.rs:529+`).

**Per-PR validation gate (all PRs):**
- `cargo test --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --check`
- Manual smoke per `docs/smoke_protocol_2026-05-01.md`.

---

## 6. Risks

- **Rewind clobbers user-made edits.** If the user (or an external
  tool) changed a tracked file on disk after the checkpoint,
  blind restore destroys work. *Mitigation:* refuse with
  `CheckpointError::WorkingTreeDrifted { path }` when on-disk
  content hashes differ from the most recent captured blob for that
  path; require explicit re-issue to force. Tested by
  `checkpoint_restore_refuses_when_working_tree_drifted`.
- **Untracked mutations escape rewind.** `bash` mutations
  (`mv`, `>`, `rm`) are not in the `write`/`edit` tracked set
  (`lib.rs:301-304`), so rewind won't undo them. *Mitigation:*
  documented limitation (§8); matches the bounded file-op tracking
  pi/anie already do. Punt bash-mutation tracking.
- **Snapshot disk growth.** Content addressing dedups identical
  blobs, but a long session that rewrites large files many times
  accumulates blobs. *Mitigation:* the sidecar dir is per-session
  and removable; add a cheap cap (keep blobs referenced by the
  manifest only) — GC unreferenced blobs on session open. Punt a
  configurable size ceiling unless smoke shows growth.
- **Protocol struct duplication.** `SessionSummary` partially
  duplicates `SessionInfo` (`lib.rs:413-429`). *Mitigation:* this
  is an intentional, documented deviation (keeps
  `anie-protocol` free of `anie-session`); the map lives at one
  controller boundary. The two shapes are small and unlikely to
  drift.
- **Turn-granular vs per-edit checkpoints.** A turn that makes many
  edits collapses to one restore point. *Mitigation:* accepted
  deviation from Claude Code (§2.2); turn granularity matches
  anie's session-entry model and keeps us out of the
  plan-10-reserved hook seam (`hooks.rs:1-9`). Per-edit capture is
  a later refinement if the hook API is ever made public.
- **Schema bump blast radius (C only).** Isolated to PR 8 with a
  forward-compat test; the picker + rewind ship at schema v4.

---

## 7. Exit criteria

- [ ] `/session` (no arg) opens an interactive, searchable session
      picker; `Enter` switches, `Esc` cancels (SESSION-1).
- [ ] `/session list` and `/session <id>` text paths still work.
- [ ] `/rewind` lists prior user turns + named checkpoints and
      restores **both** the transcript and the working tree to the
      selected point (SESSION-3, SESSION-5).
- [ ] `/checkpoint [name]` records a labeled anchor.
- [ ] Rewind refuses when a run is active and when a tracked file
      has drifted on disk (typed error, no silent clobber).
- [ ] No new third-party crate added; `sha2` reused from the
      workspace (`Cargo.toml:66`).
- [ ] Rewind/picker land at session schema v4 (no bump);
      *optional* PR 8 bumps to v5 with a forward-compat test.
- [ ] `cargo test --workspace` green; `cargo clippy --workspace
      --all-targets -- -D warnings` clean; `cargo fmt --check` clean.
- [ ] Manual smoke (`docs/smoke_protocol_2026-05-01.md`): start a
      session, let the agent edit ≥2 files across ≥2 turns,
      `/rewind` to the first turn, confirm files revert and the
      transcript shrinks; `/session` reopens and switches cleanly.
- [ ] `docs/arch/anie-rs_architecture.md` documents the picker
      data-flow and checkpoint store layout.
- [ ] `docs/ROADMAP.md` marks initiative #8 (picker + rewind)
      landed; `docs/notes/commands_and_slash_menu.md` flips the
      relevant "Not implemented" rows.

---

## 8. Deferred

- **SESSION-2 — tree-overlay branch visualization.** The
  `tree.rs` stub (`overlays/tree.rs:1-40`) stays a placeholder.
  Branch *relationships* are low-value next to rewind, and the
  data layer is half-missing: `SessionInfo` (`lib.rs:413-429`)
  doesn't even surface `parent_session` (it's read and discarded
  in `list_sessions`, `lib.rs:1002-1010`). Revisit only after the
  picker + rewind prove the session-navigation UX is used.
- **Per-edit checkpoint granularity.** Requires exposing the
  `pub(crate)` tool-hook seam (`hooks.rs:1-9`,
  `agent_loop.rs:1466/1519`), which is reserved for the
  out-of-process extension system (plan 10). Turn granularity
  ships now; per-edit waits on that API.
- **Bash-driven mutation tracking for rewind.** Only
  `write`/`edit` paths are tracked (`lib.rs:301-304`). Capturing
  arbitrary shell-driven file changes would need a sandbox/FS-watch
  layer (rival analysis initiative #7) — out of scope here.
- **Cross-session / git-level restore.** No `git2`/`gix`
  dependency; the shadow store is self-contained and works in
  non-git working dirs. A git-integration mode is a separate
  initiative if ever wanted.
- **Configurable snapshot retention ceiling.** Start with
  manifest-referenced GC on open; add a size cap only if smoke
  shows real growth.
- **SESSION-4 if the optional PR 8 is dropped.** If the schema bump
  is undesirable in this cycle, fork branch summarization defers
  cleanly — nothing in workstreams A/B depends on it.
