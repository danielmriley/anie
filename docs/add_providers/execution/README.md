# Execution plans — add_providers rollout

The per-provider specs in `../` describe **what** to build. These
files sequence the work across PRs and track cross-plan
dependencies so nothing gets double-done.

## Why a second tier of plans

The provider specs (`../00`–`../06`) are specification-level:
scope, model catalogs, round-trip contracts, exit criteria.
Several of them share infrastructure — the per-model compat
blob, `ThinkingRequestMode::NestedReasoning`, `thought_signature`
storage on `ContentBlock`, the preset catalog — and that shared
work has to land once, in one specific order, before any of the
provider plans can start.

These execution docs:

- Sequence phases across all seven plans into PR-sized milestones.
- Pull cross-plan foundation work into its own dedicated phase 0
  so plans 01 / 02 / 05 aren't each re-litigating the same
  scaffolding.
- Break the less-structured plans (01, 02, 05) into explicit
  phases with test gates; plans 03 / 04 / 06 already have phased
  breakdowns inside their own spec files so their execution
  docs cross-reference those.

## Master milestone sequence

Read top-to-bottom. Every milestone is ≤ 5 files touched and has
a test gate. Nothing marked "merge" ships until `cargo test
--workspace` is green plus `cargo clippy --workspace
--all-targets` is clean.

| # | Milestone | Plan | PRs | Unblocks |
|---|---|---|---|---|
| 0 | [Foundation: compat blob on Model, NestedReasoning variant, `thought_signature` prep](00_foundation.md) | cross | 3 | 01, 02, 03, 05 |
| 1 | [Provider selection UX: preset catalog + category picker](01_ux_prerequisite.md) | `../00` | 2 | every provider plan |
| 2 | [OpenRouter](02_openrouter_phases.md) | `../01` | 2 | nothing (independent) |
| 3 | [OpenAI-compat batch: Mistral → xAI → Groq → Cerebras](03_openai_compat_phases.md) | `../02` | 4 | nothing (independent) |
| 4 | [Google Gemini phase A–C](../03_google_gemini.md#implementation-phases) | `../03` | 3 | nothing (independent) |
| 5 | [OpenAI Responses phase A–E](../04_openai_responses_api.md#implementation-phases) | `../04` | 5 | unblocks Azure+Responses follow-up |
| 6 | [Azure OpenAI](06_azure_phases.md) | `../05` | 2 | nothing (independent) |
| 7 | [Amazon Bedrock phase A–D](../06_amazon_bedrock.md#implementation-phases) | `../06` | 4 | nothing (independent) |

Total: roughly 25 PRs if everything goes through. Each row is
one or more PRs; the linked phase doc specifies the split
inside each.

## Critical dependencies

```
┌─────────────────────────────────────────────────────┐
│ 0. Foundation                                       │
│    - Model.compat blob                              │
│    - ThinkingRequestMode::NestedReasoning           │
│    - ContentBlock::Thinking.thought_signature prep  │
└─────────────────────────────────────────────────────┘
           │                │
           ▼                ▼
┌──────────────────────┐ ┌──────────────────────┐
│ 1. UX prerequisite   │ │ 4. Gemini            │
│    ProviderPreset    │ │    (needs            │
│    + category picker │ │    thought_signature)│
└──────────────────────┘ └──────────────────────┘
           │
    ┌──────┴─────┬──────┬───────────┬──────┐
    ▼            ▼      ▼           ▼      ▼
┌─────────┐ ┌─────────┐ ┌─────────┐ ┌──────┐ ┌─────────┐
│ 2. OR   │ │ 3. Batch│ │ 5. Rsp. │ │ 6.Az │ │ 7.Bedrk │
└─────────┘ └─────────┘ └─────────┘ └──────┘ └─────────┘
                                        │
                        ┌───────────────┘
                        ▼
                 (Azure+Responses
                  follow-up after
                  both 5 and 6 land)
```

Milestones 2–7 are **parallelizable** across multiple engineers
or agents after milestones 0 and 1 land. On a solo schedule,
sequential in priority order is the simplest: 0, 1, 2, 3, 4, 5,
6, 7. Plan 01 (OpenRouter) is the highest-value landing, so
after milestones 0 and 1 are in, go there next.

## Parallelism boundaries within a milestone

Several milestones have phases that are internally sequential
(e.g. Gemini phase C depends on phase B's parser existing).
Others are batch work where sub-providers are independent (xAI
doesn't need Mistral). The phase docs call out which case each
is.

## Per-milestone exit gates

Every milestone ends with:

- [ ] `cargo test --workspace` green
- [ ] `cargo clippy --workspace --all-targets` clean
- [ ] No new `fs::write`, `unwrap()`, or `.expect()` in
      production code (per CLAUDE.md)
- [ ] Provider's catalog entries appear in `/providers` add
      picker's category group
- [ ] Invariant suite exercises the new provider with one
      cross-provider fixture
- [ ] (If API key available) manual two-turn smoke against
      the real endpoint logged in the PR description

Milestones that ship provider code without a live smoke are
allowed to merge behind the feature; the smoke result gets
appended to the PR's comment trail once a user with a key
verifies.

## Spec vs execution — where to write changes

Plan refinements, new out-of-scope items, or corrections
based on implementation learnings go back into the **spec**
file under `../`. These execution docs only track milestones
and cross-plan dependencies. When a phase completes, check it
off in the corresponding spec file's exit criteria; do not
duplicate that state here.

If a phase discovers a new shared dependency, it goes in the
foundation phase (milestone 0) as a late-add with a date and
reason, and the unblocked milestones adjust.

## What these docs deliberately don't cover

- **Timeline estimates.** Effort labels (S/M/L) in the spec
  files are rough and not claimed to be calendar time.
- **Code review logistics.** Who reviews what is out of scope
  here.
- **Release cadence.** Whether milestones ship as separate
  minor versions or get bundled is a release-engineering
  question, not a planning one.
