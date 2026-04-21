# Plan 02 — built-in search tools: `grep`, `find`, `ls`

**Tier 2 — medium cost, clearly scoped, user-facing win.**

pi ships seven tools: `read`, `bash`, `edit`, `write`, `grep`,
`find`, `ls` (`packages/coding-agent/src/core/tools/index.ts:110`).
anie ships four: `read`, `bash`, `edit`, `write`
(`crates/anie-tools/src/lib.rs`). The agent can shell out via
`bash` for search, but structured grep output is faster to iterate
on and cheaper on tokens (the bash output for `grep -r` includes
ANSI and shell noise; a first-class tool returns clean structured
results).

## Rationale

Three concrete reasons a native grep/find/ls beats `bash`:

1. **Cross-platform.** `grep -r` flags differ between GNU and BSD.
   A Rust impl built on `grep-searcher` (the ripgrep library) is
   identical everywhere.
2. **Structured output.** Returns `{ path, line, column, matched
   text }` tuples, which the model can act on deterministically
   without parsing shell output.
3. **Sandboxing.** `ls` through bash could be a vector for
   arbitrary paths; a native tool with a cwd guard is clearer.

Pi's implementations are pragmatic (their `grep` wraps
`child_process.spawn("rg", ...)` with argument validation). Anie
can do the same or pull in `grep-searcher` / `ignore` crates —
both are rock-solid and widely used.

## Design

### `grep` tool

Wraps `ripgrep` functionality (either via shelled-out `rg` or via
the `grep-searcher` crate). Schema:

```json
{
    "type": "object",
    "properties": {
        "pattern": {"type": "string"},
        "path": {"type": "string", "description": "File or directory to search"},
        "glob": {"type": "string", "description": "Glob filter (*.rs etc.)"},
        "type": {"type": "string", "description": "File-type filter (rg --type)"},
        "case_insensitive": {"type": "boolean"},
        "output_mode": {
            "type": "string",
            "enum": ["content", "files_with_matches", "count"]
        },
        "limit": {"type": "integer"}
    },
    "required": ["pattern"]
}
```

Output format: for `content` mode, `{path}:{line}:{match_text}`
lines; for `files_with_matches`, one path per line; for `count`,
`{path}:{count}` lines. Truncate at `limit` (default 250 lines)
with a "remaining N lines not shown" footer matching the read-tool
convention.

### `find` tool (aka `glob`)

Wraps glob-based file finding — not Unix `find`. Schema:

```json
{
    "type": "object",
    "properties": {
        "pattern": {"type": "string", "description": "Glob (src/**/*.rs)"},
        "path": {"type": "string", "description": "Search root"},
        "limit": {"type": "integer"}
    },
    "required": ["pattern"]
}
```

Returns one path per line, respecting `.gitignore` by default
(same as ripgrep).

### `ls` tool

Directory listing. Schema:

```json
{
    "type": "object",
    "properties": {
        "path": {"type": "string"},
        "show_hidden": {"type": "boolean"}
    },
    "required": ["path"]
}
```

Output: one entry per line, with `/` suffix on directories, `*`
suffix on executables. Respects the cwd guard like `read` does.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-tools/Cargo.toml` | Add `grep-searcher`, `ignore` deps (or `tokio::process` if shelling to `rg`). |
| `crates/anie-tools/src/grep.rs` | New. |
| `crates/anie-tools/src/find.rs` | New. |
| `crates/anie-tools/src/ls.rs` | New. |
| `crates/anie-tools/src/lib.rs` | Re-export. |
| `crates/anie-cli/src/bootstrap.rs` | Register new tools. |
| `.claude/skills/adding-providers/SKILL.md` | Mention that anie now ships search tools (nice-to-have, not blocking). |

## PRs

### PR A — `grep` tool

1. Add `grep-searcher = "0.1"` + `ignore = "0.4"` to
   `anie-tools/Cargo.toml`. Decide up front: native library or
   shelled-out `rg`.
   - **Recommendation:** native library. ripgrep's CLI is mostly
     a wrapper over `grep-searcher`; using the library avoids
     the "is `rg` on PATH?" problem.
2. `grep.rs` implements the `Tool` trait:
   - Validates `path` against the cwd guard.
   - Builds a `grep_searcher::Searcher` with the requested
     regex, case-sensitivity, and glob filter.
   - Iterates matches, writes into a `String` buffer, truncates
     at `limit`.
   - Returns `ToolResult { content: [Text { text }], details: ... }`.
3. Test with a temp-dir fixture: create files, run the tool
   through its `execute()`, assert output format and the
   truncation footer.
4. Register in `build_tool_registry`.

### PR B — `find` tool

1. Uses `ignore::WalkBuilder` with a glob filter via `overrides`.
2. Similar shape to grep tool; simpler output.
3. Tests: glob matching, gitignore respect, limit truncation.

### PR C — `ls` tool

1. Uses `tokio::fs::read_dir`.
2. Decorator on dir / executable / symlink entries.
3. Cwd guard.
4. Tests: normal listing, hidden files, missing dir errors
   cleanly.

### PR D (optional) — surface through slash commands

For parity with pi's auto-complete: expose `/grep`, `/ls`,
`/find` slash commands that route through the same tools. Low
priority; the agent calls them via tool-use.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `grep_finds_matches_in_content_mode` | `anie-tools/src/grep.rs` |
| 2 | `grep_files_with_matches_mode_lists_paths_only` | same |
| 3 | `grep_count_mode_returns_path_count_pairs` | same |
| 4 | `grep_truncates_at_limit_with_footer` | same |
| 5 | `grep_respects_gitignore_by_default` | same |
| 6 | `find_walks_glob_patterns` | `anie-tools/src/find.rs` |
| 7 | `find_respects_gitignore_by_default` | same |
| 8 | `ls_lists_directory_contents` | `anie-tools/src/ls.rs` |
| 9 | `ls_refuses_path_outside_cwd_guard` | same |
| 10 | `tools_are_registered_in_default_registry` | `anie-cli/src/bootstrap.rs` or integration tests |

## Risks

- **ripgrep's transitive deps** could pull in a lot. `grep-searcher
  + ignore` together are well under 100k LoC and widely vetted.
  The alternative (shelling to `rg`) is smaller but ties us to
  whatever ripgrep is on PATH.
- **Path-traversal correctness.** Every tool must validate the
  input path against the session cwd (same policy `read` uses).
  Copy that guard exactly; don't reinvent.
- **Large repos + `grep`**: 250-line limit matches existing
  read-tool defaults. If users complain, bump.

## Exit criteria

- [ ] Three tools ship, registered by default.
- [ ] Cwd guards equivalent to `read`.
- [ ] Tests 1-10 pass; `cargo clippy --workspace --all-targets
      -- -D warnings` clean.
- [ ] Manual: "find all TODOs in the project" via agent-driven
      tool call completes without a `bash` fallback.

## Deferred

- **Web fetch tool** (pi has one). Adds a network dependency to
  the tool layer; tighter security story needed first.
- **Git tool** (pi does not ship one as an explicit tool either
  — it's `bash`).
