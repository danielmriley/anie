# Refactor plans — comparison against pi-mono

This doc sits alongside the refactor plans. It compares what plans
00–08 propose against how pi-mono (`~/Projects/agents/pi/`) is
actually structured, given that anie's intent is to be "essentially
a Rust clone of pi-mono."

Two things worth noting up front:

1. pi-mono has its own Rust port plan at
   `~/Projects/agents/pi/docs/rust-agent-plan.md`. It proposes a
   five-crate layout (`agent-ai`, `agent-core`, `agent-cli`,
   `agent-extensions`, `agent`) and hard-specific dep choices.
   anie's current 12-crate layout is more granular than pi's
   proposal. Not wrong — but it's worth knowing pi itself suggested
   five, not twelve.
2. Some things pi does, anie doesn't attempt yet. Several refactor
   plans become easier (or unnecessary) when the missing features
   land. This doc flags those.

## Package / crate inventory

| Concern | pi (TS) | anie (Rust) |
|---|---|---|
| LLM providers | `packages/ai` (17 provider files) | `anie-provider` + `anie-providers-builtin` (2 providers) |
| Agent loop | `packages/agent` (agent-loop.ts 636, agent.ts 543) | `anie-agent/agent_loop.rs` (1083) |
| Coding-agent shell | `packages/coding-agent/src/core/*` (~14 k LOC) | `anie-cli` + `anie-auth` + `anie-config` + `anie-session` + `anie-tools` |
| TUI framework | `packages/tui` (differential renderer, ~11 k LOC) | `ratatui` dependency, `anie-tui` (own layer, ~7 k LOC) |
| Extensions | `packages/coding-agent/src/core/extensions` (3099 LOC, real) | `anie-extensions` (4 LOC, stub) |
| Skills | `core/skills.ts` (508) | — |
| Prompt templates | `core/prompt-templates.ts` (296) | — |
| Settings | `core/settings-manager.ts` (970) | Spread across `anie-config` + onboarding UI |
| Compaction | `core/compaction/` (1355) | Fused into `anie-session` + controller |
| Slash commands | 21 builtins + extension/prompt/skill-registered | ~12 builtins, no registry |
| Web UI | `packages/web-ui` | — |
| Slack bot | `packages/mom` | — |
| GPU pod mgr | `packages/pods` | — |

For the Slack bot / web UI / pod manager, anie is not trying to
match. The others are the scope of this comparison.

## Overall shape of the comparison

pi-mono is larger than anie by roughly 3–4x in the areas anie
targets. anie is younger. Most of what the refactor plans flag as
"growing pressure points" corresponds to things pi either solved
differently or hasn't solved either:

- pi has the same god-object problem —
  `core/agent-session.ts` is **3076 lines**, bigger than anie's
  `controller.rs` (1967). pi's interactive mode
  (`modes/interactive/interactive-mode.ts`) is **4999 lines**,
  bigger than anie's `onboarding.rs` (2312).
- pi solved the overlay-screen problem by having **~30 small
  component files** in `modes/interactive/components/`, each 50–1200
  LOC, one per selector/overlay. This is exactly the shape plan 02
  proposes (`OverlayScreen` trait + per-overlay files).
- pi has the same provider-file size problem —
  `anthropic.ts` 954, `openai-completions.ts` 891,
  `openai-codex-responses.ts` 972, `google-gemini-cli.ts` 996,
  `amazon-bedrock.ts` 891. pi chose "many moderately-big files" over
  "one shared base." anie's `openai.rs` at 2084 is large because it
  absorbs what pi splits across `openai-completions.ts` +
  `openai-responses.ts` + `openai-responses-shared.ts`.
- pi uses a shared `providers/openai-responses-shared.ts` (536 LOC)
  only for the OpenAI-Responses lineage, not across all providers.
  Plan 04 proposes a workspace-wide `ProviderRequestBuilder`, which
  is **more** unification than pi does. Consider whether that's
  deliberate.

## Plan-by-plan comparison

### Plan 00 — CI enforcement

**Pi equivalent:** pi runs `biome check --write --error-on-warnings
. && tsgo --noEmit && npm run check:browser-smoke && cd
packages/web-ui && npm run check` as its check gate (see
`package.json` root `scripts.check`). It also has `husky` for
pre-commit.

**Assessment:** Aligned with pi's stance (lint errors block), just
applied to the Rust toolchain. No change needed to the plan.

### Plan 01 — `openai.rs` module split

**Pi equivalent:** pi splits the OpenAI lineage across **three
files plus a shared base**:

- `providers/openai-completions.ts` (891 LOC)
- `providers/openai-responses.ts` (264 LOC)
- `providers/openai-responses-shared.ts` (536 LOC)
- `providers/openai-codex-responses.ts` (972 LOC)
- `providers/azure-openai-responses.ts` (253 LOC)

Plus supporting modules: `transform-messages.ts` (160) for
protocol conversion used by every OpenAI-shaped provider, and
`utils/event-stream.ts` for streaming primitives.

pi has never had a `TaggedReasoningSplitter`-style thing — its
reasoning handling is per-provider, mostly in the respective
`stream*` functions.

**Assessment:** Plan 01's extraction targets (tagged reasoning,
streaming, convert, reasoning_strategy) are a **closer match to
pi's transform-messages.ts + event-stream.ts split** than to the
current anie monolith. After plan 01:

- `openai/tagged_reasoning.rs` ≈ pi's per-provider reasoning parsing
  (no direct pi parallel — deliberate anie-specific logic because
  anie supports local `<think>` tag providers that pi doesn't
  prioritize).
- `openai/convert.rs` ≈ pi's `transform-messages.ts`.
- `openai/streaming.rs` ≈ pi's `utils/event-stream.ts`.
- `openai/reasoning_strategy.rs` — no direct pi parallel; pi hard-codes
  reasoning behavior per provider file rather than computing it.

**Gap the plan doesn't cover:** pi also has an OpenAI **Responses
API** path (different from Completions). anie only implements
Completions. Splitting `openai.rs` into `openai/responses.rs` and
`openai/completions.rs` isn't in plan 01 because anie doesn't have
Responses support. When it does, the file-per-API split will match
pi's shape.

**Suggested plan addition:** once OpenAI Responses support lands
(or at a minimum, once more providers exist), the "`openai/mod.rs`"
facade becomes an `openai/` directory with one file per API. This
is the pi shape.

### Plan 02 — TUI overlay trait + shared widgets

**Pi equivalent:** pi's differential-rendering TUI has a
`Component` interface and every selector/overlay is a small
component file in `modes/interactive/components/`:

- `model-selector.ts` (338)
- `session-selector.ts` (1010) + `session-selector-search.ts` (194)
- `settings-selector.ts` (444)
- `config-selector.ts` (592)
- `oauth-selector.ts` (121)
- `login-dialog.ts` (178)
- `theme-selector.ts` (67)
- `thinking-selector.ts` (74)
- `scoped-models-selector.ts` (341)
- `tree-selector.ts` (1239)
- `extension-selector.ts` (107) + `extension-editor.ts` (147) + `extension-input.ts` (87)
- `user-message-selector.ts` (143)
- `show-images-selector.ts` (not sized above)

The component interface is essentially `render` + `handleInput` +
lifecycle. Each selector is a file. No enum-dispatched mega-overlay.

**Assessment:** Plan 02 proposes an `OverlayScreen` trait with
`handle_key`/`handle_tick`/`handle_worker_event`/`render`. This is
exactly what pi does. The shape is right.

**Gap the plan doesn't cover:** anie currently has only **two**
overlay screens (onboarding + providers). pi has **14+** small
overlays. The plan focuses on deduplicating the two; it doesn't
plan the directory structure for 14 of them.

**Suggested plan addition:** after phases 1–3 of plan 02 land, add
a skeleton for future overlays:

```
crates/anie-tui/src/overlays/
├── mod.rs
├── onboarding.rs           (migrated)
├── providers.rs            (migrated)
├── model_picker.rs         (already exists; move here)
├── session_picker.rs       (future — pi has this)
├── theme_picker.rs         (future — pi has this)
├── settings.rs             (future — pi has this)
├── oauth.rs                (future — pi has this)
├── hotkeys.rs              (future — pi has this)
└── login.rs                (future — pi has this)
```

Establishing the directory and trait together makes adding pi-shaped
overlays cheap. This is additional structural scaffolding plan 02
should land, not new features.

**Plan 02 phase 4 (clone audit + typed provider keys):** no pi
parallel (TS has structural typing and doesn't care). Leave as-is.

### Plan 03 — Controller decomposition

**Pi equivalent:** `core/agent-session.ts` is 3076 lines and owns
roughly the same concerns as anie's `ControllerState`. pi has **not**
split it. The code base absorbs it. What pi **does** have cleanly
separated:

- `core/compaction/` directory (1355 LOC) — plan 03 phase 2
  corresponds to this cleanly.
- `core/model-registry.ts` (844) + `core/model-resolver.ts` (628)
  — plan 03 phase 1 (`ModelCatalog`) corresponds.
- `core/slash-commands.ts` (38) — just a static array. pi does
  **not** have a rich command-registry trait; handlers live in
  `modes/interactive/interactive-mode.ts` and extensions register
  via `pi.registerCommand`.
- `core/settings-manager.ts` (970) — no anie parallel yet.

**Assessment:** Plan 03 is going **further** than pi has on the
controller split. That's not wrong, but:

- Plan 03 phase 3 ("slash-command registry") is **heavier** than pi's
  approach. pi treats builtin commands as a static array and routes
  through a single mode-level handler, while extensions register via
  the `ExtensionAPI.registerCommand`. If anie plans to land
  extensions (plan 07 option B), the registry should match that
  shape: builtins as a static table, extensions `register_command`
  into the same table at load time. That is effectively what plan
  03 phase 3 proposes, just worth aligning vocabulary.

- Plan 03 phase 5 ("split ControllerState into focused types") is
  more aggressive than pi. pi tolerated the 3000-line god object.
  Anie should not — Rust doesn't forgive it the way TS does (file
  navigation, borrow checker fighting, slower compile). Keep the
  split.

**Suggested plan addition:** phase 3's `SlashCommand` trait should
accept a `source: SlashCommandSource` (builtin | extension | prompt
| skill), matching pi's `slash-commands.ts`. Prompt templates and
skills are future anie features, but the type can carry them without
cost.

### Plan 04 — Provider HTTP + discovery unification

**Pi equivalent:** pi does **not** have a unified request builder.
Each provider file builds its own request using shared helpers from
`providers/simple-options.ts` (47 LOC) and
`providers/transform-messages.ts` (160). Anthropic has its own
header/auth logic; OpenAI-group providers share via
`openai-responses-shared.ts` (536) for responses-API; Google has
`google-shared.ts` (326).

pi's model discovery is different: models are **baked into** the
code as `models.generated.ts` (14431 LOC — a generated registry).
Dynamic discovery exists for OpenAI-compatible `baseUrl`-driven
scenarios but is lightweight.

**Assessment:** Plan 04's `ProviderRequestBuilder` **goes further
than pi**. pi deliberately keeps providers independent — each wire
protocol has enough differences (cache_control, thinking blocks,
reasoning fields, tool-call serialization, error shape) that pi
accepts duplication as the price for keeping each provider readable.

This is a real trade-off. Options:

- **Keep plan 04 as-is:** anie gets a unified builder, which reduces
  duplication at the cost of a slight abstraction overhead. Works if
  the builder is narrow (headers + auth + status classification) and
  the provider still owns request-body construction.
- **Narrow plan 04:** drop phase 1's `ProviderRequestBuilder`; keep
  phases 2–3 (shared tool-call assembler, unified discovery). Match
  pi's approach.

**Recommendation:** narrow plan 04 to "share what genuinely repeats"
and let each provider keep its own request shape. The anthropic vs
openai request-body logic diverges enough that the builder saves
little. The ToolCallAssembler (phase 2) and unified discovery
(phase 3) are genuine wins; ship those.

**Gap the plan doesn't cover:** pi's registry approach
(`ExtensionAPI.registerProvider`) is how new providers land at
runtime — the provider is configuration, not code. anie's plan has
providers as compiled code. If anie follows pi's extension model
(plan 07 option B), `registerProvider` should land as part of that.
Not this plan.

### Plan 05 — Provider error taxonomy

**Pi equivalent:** pi uses plain `Error` subclasses in a few places
and mostly-stringy errors otherwise. `isContextOverflow(err)` is a
helper. TS doesn't have an `enum`-of-causes for provider errors in
the way anie is aiming for.

**Assessment:** anie has stronger typing available than TS, and
Rust call sites benefit from `match` exhaustiveness in a way TS
couldn't. Plan 05 is **more rigorous than pi** here, and that's
appropriate — it's using Rust's capabilities. Leave as-is.

**Gap the plan doesn't cover:** pi's `isContextOverflow(err)` is
cross-provider (see `core/agent-session.ts`). Plan 05's
`ContextOverflow` variant assumes the provider's HTTP-status
classification emits it. Confirm every provider does — OpenAI's
400 with a specific body, Anthropic's 400 with a different body,
Google's similar. This is a **"classify-at-boundary" contract**
that deserves explicit tests, which plan 05 phase 2's sub-step C
covers. Keep.

### Plan 06 — Session write locking

**Pi equivalent:** pi uses `atomicwrites` (listed in `rust-agent-plan.md`
as the planned dependency) and relies on atomic file-rename writes
for the session JSONL. pi does **not** lock the file.
`session-manager.ts` (1425 LOC) opens, writes, closes per operation
mostly — it doesn't hold an open file handle.

**Assessment:** This is an area where **anie can be better than pi**.
Rust's `fd-lock` and atomic rename both exist; anie's current
append-only-file design (keeping the handle open) is different from
pi's write-per-op shape, so locking is the right fix for anie's
approach.

**Alternative to consider:** adopt pi's "open-write-close per
operation, atomic-rename" shape instead of file-locking. That would
change anie's session design more invasively. `fd-lock` is the
smaller change. Plan 06 as written is the right call.

### Plan 07 — `anie-extensions` decision

**Pi equivalent:** pi has a **fully-realized** extension system.
`coding-agent/src/core/extensions/types.ts` is **1461 lines** and
defines an `ExtensionAPI` surface with ~35 event types, tool
registration, command registration, keyboard shortcut registration,
CLI flag registration, message renderer registration, and provider
registration with OAuth support. `loader.ts` (557) discovers and
loads extensions (TypeScript, via `jiti` at runtime). `runner.ts`
(915) dispatches events to handlers with error isolation.

pi's extensions are **external process or external TS module**, not
compiled into the binary. That matches pi-mono's own
`docs/rust-agent-plan.md`:

> External process plugins: any language (TypeScript, Python, Go,
> shell), JSON-RPC 2.0

**Assessment:** Plan 07 option B ("make it real") as currently
written is **far too minimal** to match pi. It proposes four
hooks (`before_agent_start`, `session_start`, `before_tool_call`,
`after_tool_call`) with a compiled-in `trait Extension`. pi has
35+ event types **and** external-process plugins **and** tool
registration **and** command registration **and** a UI context API.

If anie is meant to be a Rust clone of pi-mono, plan 07 option A
("delete the crate") is the right interim choice — because option
B as scoped does not reach pi-parity, and landing a small
extension system then redesigning it costs more than waiting until
the full pi-shaped contract can be ported.

**Suggested plan addition:** write a new plan 07.5 or 10 —
"Extension system v1 (pi-shaped)" — that scopes a real port of
pi's extension system. Minimum it should cover:

- External-process JSON-RPC extension protocol (not compiled-in).
- ~35 event types matching pi's `ExtensionEvent` union.
- `register_tool`, `register_command`, `register_shortcut`,
  `register_flag`, `register_message_renderer`,
  `register_provider`.
- `ExtensionUIContext` with `select`, `confirm`, `input`, `notify`,
  `setStatus`, `setWidget`, `setFooter`, `custom`.
- Error isolation — a buggy extension must not crash anie.

This is a multi-week plan in its own right. Tracking it outside
plan 07 is the right shape.

**Immediate recommendation:** plan 07 option A (delete) for now.
Track the pi-shaped extension port as a separate roadmap item.

### Plan 08 — Small hygiene items

**Pi equivalent (per item):**

- Phase A (`.anie/` paths): pi uses `getAgentDir()`, `getAuthPath()`,
  `getDebugLogPath()` helpers from `config.ts`. Same shape as plan
  proposes.
- Phase B (HTTP client panics): pi doesn't have this issue — JS has
  no equivalent of `.expect`. Rust-specific.
- Phase C (`.expect` audit): Rust-specific.
- Phase D (channel-send logging): pi's event bus (`event-bus.ts`,
  33 LOC) is similarly thin; pi doesn't log on missed-listener
  cases. This is an anie improvement, not pi-parity.
- Phase E (tool registry cache): pi's tools live in a
  `wrapRegisteredTools` function that runs once per session (see
  `agent-session.ts`). Same shape.
- Phase F (borrowing context iterator): Rust-specific; TS doesn't
  care about ownership.

**Assessment:** Hygiene items are independent of pi-parity. Ship
whenever.

## Things pi has that no refactor plan covers

These aren't refactors — they're feature gaps. Listed so they're
visible alongside the plans, not to suggest landing them all
immediately:

| Feature | pi location | Status in anie |
|---|---|---|
| Skills system | `core/skills.ts` (508), `core/resource-loader.ts` (916) | Not implemented; in `docs/ideas.md` |
| Prompt templates | `core/prompt-templates.ts` (296) | Not implemented; in `docs/ideas.md` |
| Themes | `modes/interactive/theme/*` | Not implemented |
| Settings manager (interactive) | `core/settings-manager.ts` (970) + selector | Partial (config files only); `/settings` in `docs/ideas.md` |
| Session tree navigation | `core/compaction/branch-summarization.ts` (355), `components/tree-selector.ts` (1239) | Fork exists; tree navigation does not |
| OAuth / subscription login | `packages/ai/src/oauth.ts` + providers' OAuth hooks | Not implemented; in `docs/ideas.md` |
| GitHub Copilot | `providers/github-copilot-headers.ts` + OAuth | Not implemented |
| Google Gemini / Vertex | `providers/google*.ts` | Not implemented |
| Amazon Bedrock | `providers/amazon-bedrock.ts` (891) | Not implemented |
| Mistral | `providers/mistral.ts` (591) | Not implemented (use OpenAI-compat) |
| Export HTML | `core/export-html/` | Not implemented |
| Import / share session | `/import`, `/share` commands | Not implemented |
| `/copy` last assistant | static command in pi | `docs/ideas.md` |
| Scoped models (Ctrl+P cycle) | `components/scoped-models-selector.ts` (341) | Not implemented |
| `/tree` + branch navigation | `components/tree-selector.ts` (1239) | Not implemented |
| `/reload` hot-reload | builtin command | Not implemented |
| Footer / status bar data | `core/footer-data-provider.ts` (339) | Partial |
| Autocomplete / `@file` fuzzy search | `packages/tui/src/autocomplete.ts` (780), `fuzzy.ts` (133) | Not implemented |
| Bracketed paste, image rendering | `packages/tui` native | Partial |
| Web UI | `packages/web-ui` | Not implemented |
| Slack bot | `packages/mom` | Not implemented |
| Differential rendering TUI | `packages/tui` | Uses `ratatui` instead |

## What would bring the refactor plans closer to pi-shape

Prioritized:

1. **Narrow plan 04 phase 1** (drop `ProviderRequestBuilder` or
   shrink it to headers+auth+classification only). Pi deliberately
   doesn't unify request building across providers; keeping
   providers independent matches pi's shape.
2. **Extend plan 02** to create an `overlays/` directory with
   per-overlay files, ready for the 10+ future selectors pi has.
   Scaffolding is cheap; plan 02 already covers most of it.
3. **Retitle plan 07 option B as incomplete.** Recommend option A
   until a full pi-shaped extension port can be planned. Capture
   the port as a separate roadmap item (~3 weeks of work).
4. **Align plan 03 phase 3's slash-command registry** with pi's
   `SlashCommandSource` tagging (`builtin` / `extension` / `prompt`
   / `skill`) so it's ready for the skill and prompt-template
   features without churn.
5. **Add a new plan 09 — "tools parity with pi":** implement
   `find`, `grep`, `ls` tools to reach pi's built-in tool set (7
   vs anie's 4). pi's tool shapes are in
   `core/tools/{find,grep,ls}.ts`.

## TUI architecture: a deliberate divergence

pi uses its own differential-rendering TUI framework
(`packages/tui`, ~11 k LOC). anie uses `ratatui`. These are
**incompatible approaches to TUI rendering**:

- pi: incremental updates, diffing previous frame against new
  frame, writing only the minimal ANSI escape delta. Optimized for
  streaming LLM output where 99% of the screen is unchanged.
- ratatui: re-render the full frame every tick, blit the whole
  buffer.

pi's `rust-agent-plan.md` actually notes this:

> `reedline`, `crossterm`, `syntect`, `termimad`, `viuer`
> `ratatui` — Full-screen overlays only

pi's Rust plan puts `ratatui` **only for full-screen overlays**,
with the main streaming output going through `termimad` + `syntect`
+ direct crossterm. anie uses `ratatui` everywhere.

**Implication for the refactor plans:** none of the refactor plans
address this divergence, and they probably shouldn't. It's a
foundational design decision. But if anie eventually wants
pi-quality streaming (smooth scroll, differential updates), the TUI
backend has to change. Flag that as known-divergent, not as a
refactor item.

## Summary

- **Plans 00, 01, 02, 05, 06, 08:** aligned with pi's direction,
  or anie-specific improvements. Ship as written.
- **Plan 03:** heavier than pi's equivalent, but appropriate for
  Rust. Small alignment on slash-command source tagging recommended.
- **Plan 04:** narrower than currently written is probably better.
  Drop phase 1 or shrink to headers-only. Keep phases 2 and 3.
- **Plan 07:** option A for now. Schedule a separate pi-shaped
  extension-system plan later.
- **New plan needed:** tools parity (`find`, `grep`, `ls`) — small.
- **Out of scope but worth tracking:** skills, prompt templates,
  themes, settings UI, OAuth, more providers, session tree,
  export/import/share, autocomplete, scoped-models.

anie is not yet a Rust clone of pi. The refactor plans don't
attempt to close the feature gap — they reduce the cost of closing
it. Landing plans 00–08 makes the feature work above cheaper; it
does not replace that work.
