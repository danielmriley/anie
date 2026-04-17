# anie — Refactor Plans Index

These plans operationalize the findings in
`docs/project_review_2026-04-17.md` and the pi-mono comparison
in `pi_mono_comparison.md`. Each plan follows the same template
used by `docs/reasoning_fix_plan.md`:

- A short root-cause / motivation section
- Explicit phases, each touching **≤5 files**
- A **Files to change** table per phase
- Per-phase **Test plan** (numbered) and **Exit criteria**
- A **Files that must NOT change** section where relevant
- A final **Out of scope** section

## Plans

| # | Title | Scope | Payoff |
|---|---|---|---|
| 00 | [CI enforcement](./00_ci_enforcement.md) | `.github/workflows/ci.yml` | Prevents regressions in all other refactors |
| 01 | [openai.rs module split + streaming tests](./01_openai_module_split.md) | `anie-providers-builtin` | Unblocks `reasoning_fix_plan.md` Phase 1 |
| 02 | [TUI overlay trait + shared widgets](./02_tui_overlay_trait.md) *(revised)* | `anie-tui` | Deletes onboarding ↔ providers duplication; scaffolds future overlays directory |
| 03 | [Controller decomposition](./03_controller_decomposition.md) *(revised)* | `anie-cli`, small `anie-session` touch | Retires `ControllerState` God object; command registry matches pi's source-tagging |
| 04 | [Provider unification (narrowed)](./04_provider_http_unification.md) *(revised)* | `anie-providers-builtin` | Shares HTTP client + tool-call assembler + discovery; keeps provider bodies independent |
| 05 | [Provider error taxonomy](./05_provider_error_taxonomy.md) | `anie-provider`, callers | Eliminates string-typed error API |
| 06 | [Session write locking](./06_session_write_locking.md) | `anie-session` | Prevents multi-process corruption |
| 07 | [`anie-extensions` stub removal](./07_extensions_crate_decision.md) *(revised — Option A only)* | `anie-extensions`, `anie-agent` | Removes misleading placeholder; clears the way for plan 10 |
| 08 | [Small hygiene items](./08_small_hygiene_items.md) | Cross-cutting | Cheap wins |
| 10 | [Extension system (pi-shaped port)](./10_extension_system_pi_port.md) *(new)* | New `anie-extensions` from scratch | pi-parity extension system: JSON-RPC subprocesses, 35+ events, tool/command/shortcut/flag/provider/renderer registration |
| — | [pi-mono comparison](./pi_mono_comparison.md) | — | Rationale behind the revisions; maps plans against pi's architecture |

*(Plan 09 is intentionally reserved for a future "tools parity
with pi" plan — `find`, `grep`, `ls`. Not writing it now because
Daniel wants to be careful about tool additions.)*

## Revisions 2026-04-17

Following the pi-mono comparison, four plans were revised:

| Plan | Change |
|---|---|
| 02 | Added phase 6 — establish `crates/anie-tui/src/overlays/` directory and land placeholder stubs for pi's next-shaped overlays (`session_picker`, `settings`, `oauth`, `theme_picker`, `hotkeys`, `tree`). |
| 03 | Phase 3 updated — `SlashCommand` trait now carries `SlashCommandSource` (Builtin / Extension / Prompt / Skill), matching pi's `slash-commands.ts`. Prevents a second migration when extensions (plan 10) and prompt-templates / skills (future) land. |
| 04 | Narrowed — dropped the original workspace-wide `ProviderRequestBuilder`. Kept the smaller shared HTTP client + status classifier in phase 1, plus phases 2 (ToolCallAssembler) and 3 (unified discovery). Matches pi's stance of keeping provider request bodies independent. |
| 07 | Reduced to Option A only — delete the stub crate. The original "Option B: make it real" is superseded by new plan 10. |

And one new plan was added:

| Plan | Status |
|---|---|
| 10 | New — full pi-shaped extension system. Multi-phase (7 phases), ~6 weeks of focused work. Rebuilds `anie-extensions` from scratch as an out-of-process JSON-RPC system. |

## Recommended order

```
00 (CI)
  └── blocks nothing, enables everything below

01 (openai split)     ◄─ high priority: unblocks reasoning_fix_plan
  ├── 04 (provider unification, narrowed) — after 01
  └── 05 (error taxonomy) — easier after 01

02 (TUI overlay)       ◄─ high priority: stops bleeding in onboarding ↔ providers
  └── phase 6 scaffolds future overlays directory

03 (controller split)  ◄─ medium priority; independent of 01/02/04/05
  └── 08.E (cached ToolRegistry) lands here
  └── phase 3 registry shape enables plan 10 phase 4

06 (session locking)   ◄─ low priority, but small; do anytime
07 (extensions stub)   ◄─ do before plan 10 (prerequisite)
08 (hygiene)           ◄─ pick items opportunistically

10 (extension system)  ◄─ multi-week; starts after 07 lands
                            phase 1: transport
                            phase 2: events
                            phase 3: blocking events + tool registration
                            phase 4: commands/shortcuts/flags (needs plan 03 phase 3)
                            phase 5: UI context primitives
                            phase 6: message renderers + widgets
                            phase 7: provider registration (blocked on OAuth)
```

## Suggested pacing

- **Week 1:** 00 (ten minutes), then 01.
- **Week 2:** 02 (including phase 6 scaffolding).
- **Week 3:** 03 (including source-tagged registry), plus 06 and
  07 as side work.
- **Week 4:** 04 (narrowed) and 05 together (they share the
  openai.rs surface touched by 01).
- **Weeks 5–10:** plan 10 (extension system) — multi-phase.
- **Ongoing:** 08 items are pickable one at a time.

## How these plans relate to existing docs

- `docs/reasoning_fix_plan.md` — Phase 1 becomes cheaper after
  plan 01. Phase 3 already covers "scattered reasoning
  capabilities"; not duplicated here.
- `docs/integration_testing_plan.md`, `docs/testing_phases/*` —
  new unit tests added by plan 01 complement (not replace) the
  integration coverage.
- `docs/ideas.md` — features like `/settings`, inline command
  menus, skills, prompt templates, themes, OAuth, session tree
  navigation become materially cheaper after plans 02 phase 6, 03
  phase 3, and 10 land.
- `docs/arch/anie-rs_architecture.md` — updated by plan 07 (drops
  the extensions box) and plan 10 (adds the JSON-RPC extension
  subsystem).
- `~/Projects/agents/pi/docs/architecture.md` and
  `~/Projects/agents/pi/docs/rust-agent-plan.md` — pi-mono's own
  architecture doc and its Rust-port proposal. Informs these
  plans. See `pi_mono_comparison.md`.

## Not in scope for any of these plans

- Sandboxing and tool approvals (tracked separately).
- OAuth and subscription auth (tracked in `docs/ideas.md`;
  blocks plan 10 phase 7).
- New first-party features from `docs/ideas.md` (skills, prompt
  templates, themes, `/settings`, session tree navigation,
  export/import/share, autocomplete, scoped-models, `/copy`,
  `/reload`, rate-limit display) that aren't cleanup. These land
  on top of the plans here; they are not part of them.
- Adding new built-in tools (`find`, `grep`, `ls`) — deferred at
  Daniel's request; tool additions warrant individual careful
  review.
- Additional providers (Google Gemini, Vertex, Bedrock, Copilot,
  Mistral, Azure Responses) — tracked in `docs/ideas.md`.
- The TUI-framework divergence between pi's differential
  renderer and anie's `ratatui`. Flagged in
  `pi_mono_comparison.md`; foundational design, not a refactor.
- Web UI and Slack-bot equivalents of pi's `web-ui` and `mom`
  packages.
- Performance micro-optimization below the algorithmic level.
