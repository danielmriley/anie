# Plan 06 — compaction fidelity: split-turn summaries + file-op tracking

**Tier 4 — significant depth, touches persistence + compaction.**

Bring two pi-side compaction features into anie:

1. **Split-turn summarization.** When the compaction cut lands
   mid-turn, generate two summaries in parallel (main history +
   turn prefix) and merge them instead of discarding the partial
   turn.
2. **File-operation tracking.** Record which files the agent
   read / modified during the summarized interval, attach to the
   `Compaction` entry as structured details. After compaction, the
   agent still has a map of "what was touched where" without
   needing to reload full transcripts.

pi's implementation lives at
`packages/coding-agent/src/core/compaction/compaction.ts:715`
(split-turn) and the compaction entry format at
`session-manager.ts` (details payload).

## Rationale

### Split-turn summaries

Today, anie's `find_cut_point` refuses to cut inside a turn — it
walks backward to the nearest user/assistant/tool-result boundary.
For long turns with many tool calls (a common case: reading
several files, running tests), this means the cut can push far
back in history just to avoid splitting, discarding a lot of
still-useful context.

pi's fix: if the cut would land mid-turn, generate a *turn-prefix
summary* of the partial turn separately, then concatenate with the
main-history summary using a `---` separator. The kept suffix of
the turn is preserved verbatim. Net result: less context loss,
smaller summarization overhead per trigger.

### File-operation tracking

After compaction, the agent often needs to re-read files it
touched earlier in the session. Without tracking, the summary
might say "reviewed the retry-policy module" but the specific
file paths are lost. pi attaches a minimal list
(`packages/coding-agent/src/core/compaction/compaction.ts:~33`):

```ts
interface CompactionDetails {
    readFiles: string[];
    modifiedFiles: string[];
}
```

No line counts. No exit codes. No bash-command history.
Extraction only pulls from `read`, `write`, `edit` tool calls.
The path lists are also **appended to the summary text itself**
in XML-like tags (`<read-files>...</read-files>`,
`<modified-files>...</modified-files>`) so the summarizer LLM
sees the paths alongside the prose; the `details` field is a
structured mirror for programmatic access on resume.

We mirror pi's shape exactly — deliberately minimal. If a
concrete need for bash-command tracking or file-size metadata
surfaces later, add a field then. "Might be useful" doesn't
earn a slot in the schema.

## Design

### Split-turn cut detection

`find_cut_point` already returns a struct indicating where the
cut lands. Extend it to carry a `split_turn: Option<SplitTurn>`
field:

```rust
pub struct CutPoint {
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub split_turn: Option<SplitTurn>,
}

pub struct SplitTurn {
    pub turn_start_entry_id: String,
    pub prefix_entry_ids: Vec<String>,
}
```

When `split_turn.is_some()`, the compaction strategy:

1. Generates two summaries in parallel:
   - *Main-history summary* over the messages before
     `turn_start_entry_id`.
   - *Turn-prefix summary* over messages in `prefix_entry_ids`.
2. Joins them as pi does: the main summary, then
   `\n\n---\n\n**Turn Context (split turn):**\n\n`, then the
   turn-prefix summary.
3. After that, appends the file-operation lists (see below) in
   `<read-files>...</read-files>` / `<modified-files>...</modified-files>`
   blocks.
4. Uses the assembled text as the `summary` field of the
   `Compaction` entry.

The `find_cut_point` logic already knows when it's in a turn; we
just surface that info instead of backing off.

**Error-handling note.** Pi uses `Promise.all`, so if one of the
parallel calls fails, the whole compaction fails. We match —
`tokio::try_join!` short-circuits on the first error and
propagates. The retry policy treats this as a compaction failure
and escalates to the agent loop, same as today's single-summary
failure.

### File-op tracking

Add optional typed `details: Option<CompactionDetails>` to the
`SessionEntry::Compaction` variant. `CompactionDetails` is a
plain struct (not `serde_json::Value` — we want type safety):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CompactionDetails {
    /// Deduplicated paths read during the summarized interval.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub read_files: Vec<String>,
    /// Deduplicated paths written or edited during the
    /// summarized interval.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modified_files: Vec<String>,
}
```

Extraction during compaction:

- `read_files`: walk the discarded tool calls, collect
  `arguments.path` (or `arguments.file_path`) for every
  `ContentBlock::ToolCall` with `name == "read"`. Dedupe.
- `modified_files`: same but `name == "write"` or `"edit"`.

If the field is omitted from an older session file, it
defaults to `CompactionDetails::default()`. If no tools in
those categories were called, both vectors are empty and
serialization drops the field entirely via
`skip_serializing_if`.

**Also:** the extracted lists are serialized into the
`summary` text inside `<read-files>...</read-files>` /
`<modified-files>...</modified-files>` tags before being handed
to the summarizer LLM. The prompt knows to keep those sections
intact verbatim. Pi does this so the summarizer has the file
list as context when producing the prose.

### Session schema bump

Bumps `CURRENT_SESSION_SCHEMA_VERSION` to 4:

```
| 4       | `SessionEntry::Compaction.details` optional field       |
|         | for file-operation tracking. Forward- and               |
|         | backward-compatible via serde defaults.                 |
```

Older binaries reading a v4 file correctly reject via the
existing future-version guard. Newer binaries reading a v3 file
default `details` to `None` (serde `skip_serializing_if`).

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-session/src/lib.rs` | `SessionEntry::Compaction.details`, schema bump to 4, `find_cut_point` returns `SplitTurn` info, `build_context` unchanged. |
| `crates/anie-cli/src/compaction.rs` | Two-summary generation, file-op scan, details serialization. |
| `crates/anie-integration-tests/tests/session_resume.rs` | Roundtrip tests for v4. |

## Phased PRs

### PR A — schema v4 + `details` field (data model only)

1. Add optional `details: Option<serde_json::Value>` field to
   `Compaction` variant.
2. Bump schema version, add changelog row.
3. Forward-compat test: v3 file loads, `details` defaults to
   `None`.
4. Roundtrip test: v4 file with `details` populated round-trips
   through open/close.
5. No compaction logic changes — the field just exists.

### PR B — file-op extraction

1. During compaction, walk discarded entries.
2. For each `Message::Assistant` with tool calls + matching
   `Message::ToolResult`, extract:
   - `files_read`: `ContentBlock::ToolCall` where
     `name == "read"` → collect `path` arg.
   - `files_modified`: same but `name == "write"` or `"edit"`.
   - `bash_commands`: `name == "bash"` + exit code from the
     tool result.
3. Serialize as the `details` value.
4. Test: compact a small synthetic session, assert the details
   payload matches the expected file-op list.

### PR C — split-turn summarization

**Signature refactor warning.** The existing `find_cut_point`
returns a tuple `(Vec<SessionContextMessage>,
Vec<SessionContextMessage>, String)`. This plan turns it into a
struct so `split_turn` fits cleanly:

```rust
pub struct CutPoint {
    pub discarded: Vec<SessionContextMessage>,
    pub kept: Vec<SessionContextMessage>,
    pub first_kept_entry_id: String,
    pub split_turn: Option<SplitTurn>,
}
```

Every caller of `find_cut_point` needs updating (inside
`anie-session` and the `compaction.rs` strategy). Plan on this
being a ~50-line refactor with ~10 test sites updated before the
new split-turn behavior lands. Keep this as a separate commit
within PR C so the refactor is reviewable on its own.

1. Refactor `find_cut_point` to return `CutPoint` struct.
2. Update every call site + test.
3. Populate `split_turn` when the cut would land mid-turn.
4. Compaction strategy branches on `split_turn`:
   - If `None`: single summary (existing behavior).
   - If `Some`: two parallel calls via `tokio::try_join!`,
     joined per pi's format.
5. `build_context` unchanged — the compaction entry has a
   joined summary text, no special handling needed downstream.
6. Tests:
   - `find_cut_point_returns_struct_with_all_fields` (regression)
   - `compact_mid_turn_produces_two_summaries_joined_per_pi_format`
   - `compact_without_mid_turn_produces_single_summary` (regression)
   - `split_turn_summary_preserves_kept_suffix`
   - `find_cut_point_surfaces_turn_prefix_when_cut_lands_in_turn`
   - `split_turn_error_propagates_to_compaction_caller`

### PR D — expose details on resume

1. `build_context` optionally returns compaction `details`
   alongside the summary.
2. System-prompt integration: at session resume, the
   system-prompt cache can surface a "recently-touched files"
   hint. Optional polish — can defer.

## Test plan

Core tests per PR above, plus:

| # | Test | Where |
|---|------|-------|
| 1 | `schema_v4_roundtrips_with_details` | `anie-session` tests |
| 2 | `schema_v3_loads_with_details_none` | same |
| 3 | `file_op_extraction_reads_tool_args` | `anie-cli/src/compaction.rs` |
| 4 | `file_op_extraction_handles_missing_tool_results` | same |
| 5 | `split_turn_summary_joined_with_separator` | same |
| 6 | `split_turn_cut_point_preserves_turn_suffix` | `anie-session` tests |
| 7 | `full_session_compact_then_resume_roundtrip` | integration tests |

## Risks

- **Parallel LLM calls can race for rate-limit budget.** Two
  summarization calls at once might both get 429'd. Mitigation:
  our retry policy already handles rate limits with a cap; both
  requests will retry independently. If both fail, compaction
  raises the error to the agent loop which gives up the turn.
- **File-op extraction accuracy.** Our tool-name matching is
  string-exact. A user-registered custom tool named `read-lots`
  wouldn't be tracked. That's correct behavior — we only track
  known tools. Document the convention.
- **Schema bump drift.** If someone's on an old anie and syncs
  a session file generated by a newer one, they'll see the
  schema-version rejection (existing behavior). Good, expected.

## Exit criteria

- [ ] All four PRs merged.
- [ ] Manual: a 30+ message session that would span a cut gets
      compacted mid-turn, and the resumed context shows the
      joined summary + kept suffix.
- [ ] File-op details visible in the persisted session file
      (`jq` inspection of a real session).
- [ ] Forward-compat test suite passes; older binaries reject v4
      files cleanly.

## Deferred

- **Branch-leaving summarization.** pi auto-summarizes the
  abandoned branch when navigating off it. Different feature —
  belongs in a separate plan if/when we surface branch
  navigation in the UI.
- **Details-driven context seeding.** Auto-re-reading files
  listed in the compaction details on resume. Tempting but
  intrusive; let the agent decide.
