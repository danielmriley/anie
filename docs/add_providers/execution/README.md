# Execution plans — OpenRouter focus

**Current scope.** Only OpenRouter ships this iteration. Other
provider plans (`../02`–`../06`) stay in place as specs but are
deferred. The user's hardware access is OpenRouter-only, and
OpenRouter itself unlocks ~500 models across every frontier
provider via a single API key.

## Why a second tier of plans

The spec file [`../01_openrouter.md`](../01_openrouter.md) says
**what** to build. The docs here sequence **how** the work
flows across PRs, pull the cross-plan scaffolding into its own
foundation phase, and set test gates per PR.

## OpenRouter-only milestone sequence

| # | Milestone | Doc | PRs |
|---|---|---|---|
| 0 | [Foundation: `Model.compat` blob + `NestedReasoning`](00_foundation.md) | — | 2 |
| 1 | [OpenRouter: preset, discovery, upstream-aware capability mapping](02_openrouter_phases.md) | `../01` | 3 |

Total: five PRs. Gemini's foundation (`thought_signature`) is
deferred until Gemini itself lands — one less foundation PR.

## Deferred milestones

| # | Name | Why deferred | Restart trigger |
|---|---|---|---|
| 1b | Provider selection UX (preset catalog) | Only adding one provider; existing onboarding has room | When adding a second provider |
| 3 | OpenAI-compat batch | User doesn't have keys for xAI/Groq/Cerebras/Mistral yet | Demand |
| 4 | Google Gemini | Biggest lift (new API kind); no key available | Demand + key |
| 5 | OpenAI Responses API | Plan 04's encrypted reasoning work is heavy; o3 users can go via OpenRouter in the meantime | Demand |
| 6 | Azure OpenAI | Enterprise-only need | Demand |
| 7 | Amazon Bedrock | AWS SDK weight; no key available | Demand |

Each deferred plan's spec file remains intact — when the time
comes, restart from where the spec left off.

## Critical dependency graph (current scope)

```
┌─────────────────────────────────────────────────────┐
│ Milestone 0. Foundation                             │
│    PR A: Model.compat blob                          │
│    PR B: ThinkingRequestMode::NestedReasoning       │
└─────────────────────────────────────────────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────────────┐
│ Milestone 1. OpenRouter                             │
│    PR A: Preset + onboarding + discovery parser     │
│    PR B: Upstream-aware capability mapping          │
│    PR C: Model picker polish (optional, conditional)│
└─────────────────────────────────────────────────────┘
```

## Per-milestone exit gates

Every milestone ends with:

- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets` clean.
- [ ] No new `fs::write`, `unwrap()`, or `.expect()` in
      production code.
- [ ] Invariant suite exercises any new/changed code path
      with one cross-provider fixture.
- [ ] PR C's manual smoke (two-turn conversation on real
      OpenRouter) is the final gate for merging OpenRouter to
      main.

## Where to record refinements

When implementation surfaces something the spec missed, update
`../01_openrouter.md` — not this file. Execution docs track
milestones; specs track design. If a phase reveals a new
foundation dependency, add it to `00_foundation.md` with a
dated note.

## What happens after OpenRouter ships

Two branches:

1. **If users report usability issues** on search at 500+
   models, fuzzy scoring + prefix grouping lands as a small
   follow-up PR. Documented as PR C in `02_openrouter_phases.md`
   — conditionally landed.
2. **If a second provider gets prioritized**, first PR is the
   plan 00 preset-catalog refactor (the "if we're adding two
   now, might as well build the shared UX first" moment). Then
   the new provider's own milestone.

Neither branch is in scope today.
