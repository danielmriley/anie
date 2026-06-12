# Plan 04 — Context-budget discipline for small models

## 1. Rationale

Field evidence (session `0f9cd627`, qwen3.5:0.8b, see
[field notes](field_notes/2026-06-12_qwen3.5-0.8b_session.md) F2/F5):
the system prompt + catalogs consumed **11,291 tokens at turn 0**
against the rlm ceiling of 16,384 — ~69% of the model's entire working
budget spent before the conversation begins, and none of it evictable
by context virtualization. Contributors measured in that session:

- **Skills catalog**: 10 skills discovered (4 bundled + project
  `.claude/skills/` + user `~/.claude/skills/`), including a 32KB body
  that trips the soft-cap warning at every launch
  (`anie.log.2026-06-12`) and one that fails parsing at every launch.
  Skills written for *other* harnesses (Claude Code) are loaded
  verbatim into anie's catalog.
- **Context files**: the host repo's `CLAUDE.md` (~8KB) plus user-level
  instruction files — written for frontier models, weighing thousands
  of tokens.
- **rlm augment**: ~700 tokens of ledger-syntax instruction
  (`RLM_SYSTEM_PROMPT_AUGMENT`, `controller.rs:2778`) that a 0.8B model
  demonstrably cannot follow — it pastes the syntax INTO bash commands
  (field notes F2: `cat <(ls) (id=ollama_tool_call_0_1 ...)`,
  grepping the filesystem for `OLLAMA_TOOL_CALL_9`).
- Full tool catalog with multi-sentence descriptions.

A small model needs the opposite shape: a prompt measured in hundreds
of tokens, a ledger measured in lines, and instruction syntax no more
complex than the calls we actually expect it to make.

## 2. Design

Everything keys off a single new concept: a **prompt-budget tier**
derived from the effective context window (post-`/context-length`
override), NOT from model size guesses:

```rust
/// Prompt-weight tier. Small = effective window <= 32k —
/// every system-prompt token visibly displaces working room.
enum PromptTier { Small, Full }
```

computed in one place in `compose_system_prompt`'s vicinity and passed
down. Hosted/big-window behavior is byte-identical (`Full` = today).

### 2a. Compact tool catalog (Small tier)

The prompt catalog (not the wire `tools` array — that is unchanged)
renders one line per tool: name + first sentence of the description.
The PR3 example block stays — examples are the highest-value tokens we
send a small model — but drops to the 5 core tools (read, write, edit,
bash, grep) to cap its weight.

### 2b. Skills-catalog budget (Small tier)

- Catalog entries whose body exceeds the existing
  `SKILL_BODY_BYTE_WARN_THRESHOLD` are excluded from the catalog in
  Small tier (loading a 32KB body into a 16k-ceiling context can never
  be right; the `/skill:<name>` command still works for explicit user
  invocation).
- Cap the rendered catalog at N=6 entries (deterministic: bundled
  first, then project, then user — matching registry precedence) with
  one summary line for the omitted count.
- Fix independent of tier: a skill that fails parsing at every launch
  should warn once per file *content hash*, not per launch (cosmetic,
  but the log noise hides real warnings).

### 2c. Context-file budget (Small tier)

`collect_context_files` output gets a token cap (default ~1,500
tokens, env `ANIE_CONTEXT_FILES_TOKENS`): files are included in
nearness order (project first) until the cap, then a one-line note
("N context files omitted; ask the user if instructions seem
missing"). `estimate_tokens` is already available for the measuring.

### 2d. Ledger v2 — Small tier

Replace the syntax-manual approach for Small tier with a ledger the
model can only use correctly because there is no syntax to get wrong:

- The per-turn ledger lists prior calls as plain lines
  (`web_search: "time of day 2026..."`) with NO ids, NO
  `(id=...)` notation, NO scope grammar.
- The recurse instruction collapses to exactly one shape:
  `recurse {"scope": {"kind": "message_grep", "pattern": "<words>"}}`
  — the one scope the field session showed the model reaching for.
  The other four scopes stay available (wire schema unchanged) but are
  not advertised in Small tier.
- The rlm augment shrinks to <150 tokens: archive exists, don't repeat
  listed calls, search the archive with recurse message_grep, verify
  after edits.

Full tier keeps today's ledger verbatim.

### 2e. Tier-aware `keep_last_n` + ceiling sanity floor

With an 11k prompt, `DEFAULT_RLM_ACTIVE_CEILING_TOKENS = 16_384` left
~5k of working room. After 2a-2d the prompt should drop to ~2-3k for
Small tier; additionally, log a startup warning when
(estimated prompt tokens) > ceiling/2, so misconfiguration is visible
instead of silently degrading.

## 3. Files to touch

- `crates/anie-cli/src/controller.rs` (tier computation, augment
  selection)
- `crates/anie-cli/src/runtime/prompt_cache.rs` (tiered catalog
  rendering, context-file cap; cache key must include the tier)
- `crates/anie-cli/src/tool_examples.rs` (Small-tier subset)
- `crates/anie-cli/src/skills.rs` / `controller.rs` (catalog budget)
- `crates/anie-cli/src/context_virt.rs` (ledger v2 rendering behind
  the tier)
- `crates/anie-evals/scenarios/` (PR 4)

## 4. Phased PRs

**PR 13 — `local_aug/PR13: PromptTier + compact tool/skills/context budgets`**
2a + 2b + 2c + the 2e warning. No ledger changes.

**PR 14 — `local_aug/PR14: ledger v2 for the Small tier`**
2d. The riskiest piece — ships behind `ANIE_LEDGER=v1` escape hatch.

**PR 15 — `local_aug/PR15: prompt-weight metrics + eval expectations`**
`RunMetrics.prompt { system_prompt_tokens }` (schema bump, coordinate
in tracker); tighten eval scenarios with Small-tier max_tokens.

## 5. Test plan

PR 13:
- `small_tier_catalog_renders_one_line_per_tool`
- `full_tier_prompt_is_byte_identical_to_today`
- `oversized_skill_bodies_are_excluded_from_small_tier_catalog`
- `context_files_truncate_at_budget_with_omission_note`
- `prompt_cache_key_distinguishes_tiers`
- `startup_warns_when_prompt_exceeds_half_the_ceiling`

PR 14:
- `small_tier_ledger_contains_no_id_notation`
- `small_tier_recurse_instruction_advertises_only_message_grep`
- `full_tier_ledger_unchanged` (byte-compare against a fixture)
- `env_ledger_v1_restores_old_format_for_small_tier`

PR 15: schema-bump forward-compat + drift-guard updates.

## 6. Risks

- **Capability loss for mid-size models**: a 14B model can use
  `tool_result` scopes the Small tier hides. Mitigation: tier is
  window-based, not model-based; a 14B at 64k window is Full tier.
  Boundary (32k) is env-tunable.
- **Tier flapping on `/context-length` changes** invalidates the
  prompt cache — acceptable, that's what the cache stamp is for.
- **Skills users expect to see** disappear from the catalog in Small
  tier. The omission line names the count; `/skills` still lists all.

## 7. Exit criteria

- [ ] Turn-0 input tokens for the field-notes scenario drop from
      ~11.3k to under 4k in Small tier (re-run the same prompt on
      qwen3.5:0.8b and read usage from the session file).
- [ ] No ledger syntax appears inside any tool argument in a 20-turn
      Small-tier smoke (the F2 signature).
- [ ] Full-tier prompts byte-identical to pre-plan-04.
- [ ] Tests + clippy green per PR; tracker updated.

## 8. Deferred

- Relevance-ranked (embedder-backed) skill-catalog selection.
- Summarizing context files instead of truncating.
- Per-model prompt-format profiles (Qwen `/think` etc.) — follow-up
  series, unchanged.
- Dropping the wire `tools` array entries by tier (wire stays full).
