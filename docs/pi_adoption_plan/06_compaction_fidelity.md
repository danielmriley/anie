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
file paths are lost. pi attaches a structured list to the
compaction entry:

```ts
details: {
    filesRead: [{ path, line_count }],
    filesModified: [{ path, operations: ["edit" | "write"] }],
    bashCommands: [{ command, exit_code }]
}
```

The agent can re-introduce paths into context on demand without
parsing free-text summaries.

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
2. Joins with `\n\n---\n\n` separator.
3. Uses the joined text as the `summary` field of the
   `Compaction` entry.

The `find_cut_point` logic already knows when it's in a turn; we
just surface that info instead of backing off.

### File-op tracking

Add optional `details: Option<serde_json::Value>` to the
`SessionEntry::Compaction` variant. During compaction, walk the
entries being summarized and build:

```json
{
    "files_read": [{"path": "src/main.rs", "count": 3}],
    "files_modified": [{"path": "README.md", "operations": ["edit"]}],
    "bash_commands": [{"command": "cargo test", "exit_code": 0}]
}
```

Extracted from:
- `files_read`: `ToolCall` with `name == "read"` → `arguments.path`.
- `files_modified`: `name == "write"` or `name == "edit"` → path.
- `bash_commands`: `name == "bash"` → arguments.command + the
  corresponding `ToolResult`'s exit code if present.

All fields optional — if the feature is disabled or the agent
didn't use those tools, `details` serializes as `None` and is
omitted.

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

1. `find_cut_point` detects mid-turn cuts, returns `SplitTurn`
   info.
2. Compaction strategy branches on `split_turn`:
   - If `None`: single summary (existing behavior).
   - If `Some`: two parallel `tokio::try_join!` LLM calls,
     joined with `---`.
3. `build_context` unchanged — the compaction entry has a
   joined summary text, no special handling needed downstream.
4. Tests:
   - `compact_mid_turn_produces_two_summaries_joined_with_separator`
   - `compact_without_mid_turn_produces_single_summary` (regression)
   - `split_turn_summary_preserves_kept_suffix`
   - `find_cut_point_surfaces_turn_prefix_when_cut_lands_in_turn`

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
