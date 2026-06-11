# Plan 02 — Repo map + on-demand symbol retrieval

## 1. Rationale

A small model spends a large share of its limited context
*finding* code before it can change it. Today anie gives it
search primitives (`find` — glob walker over the `ignore`
crate, `crates/anie-tools/src/find.rs`; `ls`; `read` with
offset/limit, `crates/anie-tools/src/read.rs:50-51`) but no
orientation: every task starts with exploratory tool calls
that burn turns and fill the active context with raw file
dumps the rlm evictor then has to claw back.

`docs/small_model_capability_ideas_2026-04-29.md` §1 sketches
the fix — a `RepoMapPolicy` injecting a compact repo skeleton
on the first model turn (lines 44-71) — and notes small models
"drown in irrelevant context" (line 24). The sketch is design
only; **no repo map, symbol index, or tree-sitter dependency
exists in the workspace** (verified absence; `Cargo.toml` has
`ignore`, `grep-searcher`, `grep-regex`, `regex`, `syntect` —
no AST parser).

Infrastructure that already exists and should be reused:

- `BeforeModelPolicy` + `ChainedBeforeModelPolicy`
  (`crates/anie-agent/src/agent_loop.rs:383-387,427-477`) —
  the injection seam; the controller already composes a policy
  vec (rlm → verifier → budget) in `build_agent`.
- `estimate_tokens` (`crates/anie-session/src/lib.rs:1370`) —
  the budgeting primitive context_virt already uses.
- The `SystemPromptCache` mtime-stamp pattern
  (`crates/anie-cli/src/runtime/prompt_cache.rs:37,63-83`) —
  the staleness-detection template for the map cache.
- The Ollama embedder + cosine reranker
  (`crates/anie-cli/src/embedder.rs:68,100-154`;
  `context_virt.rs:997-1104`) — available for a later
  embedding-backed rerank, **not** required by this plan.

## 2. Design

### 2a. The map builder

New module `crates/anie-cli/src/repo_map.rs`:

```rust
pub(crate) struct RepoMap {
    /// Rendered, token-capped markdown skeleton.
    pub text: String,
    /// (path, mtime) stamp of every file that contributed,
    /// for staleness checks (SystemPromptCache pattern).
    stamp: Vec<(PathBuf, SystemTime)>,
}

pub(crate) fn build_repo_map(cwd: &Path, max_tokens: u64) -> RepoMap
```

Content, in priority order until the token cap:

1. **File tree** — directories + file names from the `ignore`
   walker (gitignore-aware, same crate `find` uses), depth-
   first, collapsed when a directory exceeds ~20 entries
   (`src/… (47 files)`).
2. **Signature skeleton** — for the most relevant files
   (largest + most recently modified first), top-level
   declaration headers extracted with **regex over
   `grep-searcher`**, not an AST: Rust
   (`pub fn|pub struct|pub enum|pub trait|impl`), Python
   (`def|class`), TS/JS (`export function|export class|
   function|class`). One line per symbol, `path:line` prefix.
3. **Recent git history** — `git log --oneline -10` when cwd is
   a repo (cheap orientation for "continue where we left off"
   tasks).

Token cap: `ANIE_REPO_MAP_TOKENS` (default **2000**), measured
with `estimate_tokens` on the rendered text. The ideas doc
suggested 2–4k; default to the low end for 8B-class context
windows. Regex extraction is anie-specific (deviation from
Aider-style tree-sitter maps): zero new dependencies, good
enough for headers; tree-sitter is Deferred.

### 2b. `RepoMapPolicy` (injection)

Implements `BeforeModelPolicy`. On the **first** model turn of
a run (no prior assistant message in the working context — the
step-0 check from the ideas-doc sketch, line 66), returns
`AppendMessages` with one synthetic user message:

```
<system-reminder source="repo-map">
…map text…
Use this map to go directly to relevant files. Prefer `read`
with offset/limit at the line numbers shown over exploratory
`find`/`ls` calls.
</system-reminder>
```

- `<system-reminder>` wrapping matches the rlm ledger
  convention (`context_virt.rs:1126`).
- Injected **once per run** and then left in place: as an
  early, byte-stable message it sits in the prompt prefix and
  costs nothing to re-send under Ollama's prefix cache
  (principle 3). The rlm evictor may page it out late in a
  long run like any other message; that is acceptable and
  requires no special pinning in v1.
- Composed into the existing policy vec in `build_agent`
  **before** the rlm context-virtualization policy, so the map
  is in the working set the evictor budgets against.
- Cache: built lazily at first use, rebuilt only when the
  stamp goes stale (any contributing mtime changed) — at most
  once per run boundary, never mid-turn.
- Gating: default **on** in `--harness-mode=rlm`; off
  elsewhere; `ANIE_REPO_MAP=0` disables (gating pattern of
  `should_wrap_failed_tool_results`,
  `crates/anie-cli/src/controller.rs:2816-2820`).
  `ANIE_REPO_MAP=1` force-enables in other modes for A/B runs.

### 2c. `repo_map` tool (on-demand drill-down)

The injected map is breadth-capped; the model needs a way to
drill in without `read`-ing whole files. New tool in
`anie-tools`:

```
repo_map
  path: optional string — file or directory
```

- No `path`: returns the current full map (re-orientation
  after heavy eviction; the rlm ledger can reference it).
- `path` = file: full symbol list for that file (uncapped by
  the global budget, capped per-file at ~200 lines).
- `path` = directory: tree + per-file symbol counts beneath it.

Registered only when the map policy is active, mirroring how
`SkillTool` registration is conditional on a non-empty
registry (`crates/anie-cli/src/bootstrap.rs:112-128`).

## 3. Files to touch

- `crates/anie-cli/src/repo_map.rs` (new: builder + policy)
- `crates/anie-cli/src/lib.rs` (module)
- `crates/anie-cli/src/controller.rs` (policy composition in
  `build_agent`)
- `crates/anie-cli/src/bootstrap.rs` (conditional tool
  registration)
- `crates/anie-tools/src/repo_map_tool.rs` (new) — or
  co-located in anie-cli if it needs the builder's cache;
  decide at PR time, note the choice inline
- `crates/anie-evals/scenarios/` (PR 4)

## 4. Phased PRs

**PR 1 — `local_aug/PR5: repo-map builder (tree + regex signatures + git log)`**
`build_repo_map` + cache stamp. Pure function, no wiring.

**PR 2 — `local_aug/PR6: RepoMapPolicy injection at first model turn`**
Policy + chain composition + rlm gating.

**PR 3 — `local_aug/PR7: repo_map drill-down tool`**
Tool + conditional registration.

**PR 4 — `local_aug/PR8: repo-map eval scenarios`**
Tighten the 5 existing navigation scenarios
(`crates/anie-evals/scenarios/`: `find_provider_trait`,
`locate_budget_policy`, …) with `max_tokens` /
`min_tool_calls` expectations under map-on vs map-off; add one
cold-start scenario on a non-anie fixture.

## 5. Test plan

PR 1 (unit, tempdir fixtures):
- `map_respects_token_cap_and_drops_lowest_priority_sections_first`
- `rust_python_ts_signatures_extracted_with_path_line_prefixes`
- `gitignored_and_hidden_files_are_excluded_from_the_tree`
- `oversized_directories_collapse_to_a_count_line`
- `stamp_detects_modified_added_and_deleted_files`
- `non_git_directory_omits_history_section_without_error`

PR 2:
- `map_is_injected_exactly_once_on_first_model_turn`
- `second_turn_context_contains_the_same_map_bytes` (prefix
  stability)
- `policy_is_noop_outside_rlm_unless_force_enabled`
- `map_injection_composes_with_context_virtualization_policy`
  (chained order; map counted by the evictor's budget)

PR 3:
- `repo_map_tool_returns_per_file_symbols_for_a_file_path`
- `repo_map_tool_unknown_path_errors_with_actionable_message`
- `tool_is_unregistered_when_map_policy_is_disabled`

PR 4:
- corpus passes with expectations tightened; record map-on vs
  map-off `tokens.total_tokens` and `tools.calls` deltas in
  the execution tracker.

## 6. Risks

- **Map eats the budget on small `num_ctx`.** 2k tokens is
  ~12% of a 16k window. Mitigation: cap is env-tunable; PR 4
  must show net token *savings* (fewer exploration calls) or
  the default shrinks.
- **Regex signatures are wrong/noisy for exotic code.** They
  only need to be good enough to point `read` at the right
  region; `path:line` lets the model verify cheaply. Tree-
  sitter upgrade is Deferred with a clean seam (builder is a
  pure function).
- **Stale map after model edits files mid-run.** v1 accepts
  staleness within a run (the model just edited the file — it
  knows). Rebuild-on-stamp-change at run boundaries only.
- **Performance on huge repos.** The walker is the same one
  `find` uses with a 1000-result cap precedent; builder caps
  walked entries (e.g. 5000) and degrades to tree-only.

## 7. Exit criteria

- [ ] All four PRs landed; tests + clippy green per PR.
- [ ] On the navigation corpus with qwen3:8b in rlm mode,
      map-on shows fewer tool calls and lower total tokens
      than map-off on ≥3 of 5 scenarios, with no pass-rate
      regression.
- [ ] First-turn request body inspected in a live smoke: map
      present once, byte-stable across turns 2–3.
- [ ] Non-rlm hosted runs byte-identical to today.

## 8. Deferred

- Tree-sitter-based extraction (new dependency; revisit if
  regex precision becomes the measured bottleneck).
- Embedding-indexed file chunks + semantic `code_search` tool
  (reuses `OllamaEmbedder`; large surface — own plan).
- Map-aware relevance paging (let `page_in_relevant` treat map
  entries as candidates).
- Pinning the map against rlm eviction (only if smoke shows
  early eviction hurts).
- PageRank-style symbol ranking (Aider does this; our
  recency+size heuristic first).
