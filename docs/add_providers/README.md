# Add providers — planning index

This folder tracks the work of expanding anie's built-in provider
set. The mechanical how-to — capability declaration, stream
parser, error classifier, invariant tests, etc. — lives in
`.claude/skills/adding-providers/SKILL.md`; these plans are the
**what** (which providers, in what order, with which model
catalog) and the **where** (onboarding, `/providers` overlay,
TUI picker, config surface).

Background material:

- **[`docs/notes/provider_expansion_and_auth.md`](../notes/provider_expansion_and_auth.md)** —
  the original brainstorm that this folder operationalizes.
- **[`.claude/skills/adding-providers/SKILL.md`](../../.claude/skills/adding-providers/SKILL.md)** —
  the six-step landing recipe every plan here cross-references.
- **[`docs/completed/api_integrity_plans/`](../completed/api_integrity_plans/)** —
  the replay-fidelity and capability-routing plans that made
  adding providers tractable. Read plan `00_principles.md` before
  touching any provider code.
- **[`docs/arch/pi_summary.md`](../arch/pi_summary.md)** — pi's
  provider set is the reference model. Its `Api` union at
  `packages/ai/src/types.ts` is the breadth target.
- **[`pi_comparison.md`](pi_comparison.md)** — per-plan
  comparison of our approach against pi's shipping
  implementation. Notes the findings from reading pi that
  updated each plan (OpenRouter reasoning shape, Gemini
  `thoughtSignature`, Bedrock bearer-token auth, Mistral
  native-vs-compat tradeoff).

## Today's built-in set

| Provider | API kind | Auth | Status |
|---|---|---|---|
| Anthropic | `AnthropicMessages` | API key | shipped |
| OpenAI (Chat Completions) | `OpenAICompletions` | API key | shipped, gpt-4o + o4-mini in catalog |
| Local OpenAI-compatible (Ollama, LM Studio, vLLM) | `OpenAICompletions` | none | shipped via auto-discovery |

`ApiKind` already carries two additional stubbed variants —
`OpenAIResponses` and `GoogleGenerativeAI` — with no provider
implementations behind them. Those are the first two slots this
folder expands into.

## Priority ordering

Ordered for impact per unit of effort. The ordering also mirrors
implementation complexity: plan 1 is the most valuable and the
fastest to land; plan 6 is the most invasive and lowest-priority.

| # | Plan | Why now | Effort |
|---|---|---|---|
| 1 | [OpenRouter](01_openrouter.md) | One API key unlocks ~200 models across providers; the user's top-line request. Pure OpenAI-compat + aggregator-specific headers. | S |
| 2 | [OpenAI-compatible batch (xAI, Groq, Cerebras, Mistral)](02_openai_compat_batch.md) | Same wire protocol as OpenAI, differ only in base URL, model catalog, and a couple of auth quirks. Batchable into one PR's worth of work. | S-M |
| 3 | [Google Gemini](03_google_gemini.md) | First non-OpenAI, non-Anthropic native protocol. Unlocks Gemini Flash/Pro for users who already pay for Google AI Studio. Requires `GoogleGenerativeAI` provider module from scratch. | M |
| 4 | [OpenAI Responses API](04_openai_responses_api.md) | Native path for o1/o3/Codex with encrypted reasoning. Requires extending `ReplayCapabilities` to cover `supports_encrypted_reasoning` and writing a new streaming parser. | M |
| 5 | [Azure OpenAI](05_azure_openai.md) | Enterprise users. Same wire as OpenAI but with deployment-name routing and different auth headers. | S-M |
| 6 | [Amazon Bedrock](06_amazon_bedrock.md) | Broad model access on AWS infra. Most complex auth (SigV4) and new streaming shape. | L |

## Shared prerequisite

Every plan in this folder depends on:

- **[`00_provider_selection_ux.md`](00_provider_selection_ux.md)** —
  once the built-in set grows past five, the current onboarding
  "pick a provider" picker and the `/providers` overlay need a
  search-first presentation. This is the shared UX work that every
  new provider would otherwise re-litigate.

Land 00 first, then the provider plans can ship independently in
any order (respecting their dependency notes).

## Execution sequencing

The specs above describe **what** to build. The
[`execution/`](execution/README.md) folder sequences the work
across PRs and tracks cross-plan dependencies — including a
dedicated "Milestone 0 foundation" for the scaffolding several
plans share (compat blob on `Model`,
`ThinkingRequestMode::NestedReasoning`,
`ContentBlock::Thinking.thought_signature`).

Start with [`execution/README.md`](execution/README.md) for the
master milestone sequence.

## Conventions used in these plans

Each per-provider plan follows this structure:

1. **User value** — why a user would pick this over what's already
   there.
2. **Wire protocol** — which existing `ApiKind` the provider
   belongs to, or what new one it introduces.
3. **Auth shape** — API key / OAuth / cloud IAM / subscription,
   plus any header-name quirks.
4. **Model catalog entries** — concrete rows with pricing, context
   window, reasoning capabilities, and replay capabilities.
5. **Onboarding integration** — provider name, preset base URL,
   discovery URL (if any), whether it goes in the "API Key" or
   "Custom" bucket in the `/providers` overlay.
6. **Capability quirks** — anything that deviates from the
   reference provider for that API kind.
7. **Exit criteria** — the six-step recipe's "done" checklist
   applied to this provider.

If a plan departs from the recipe, the deviation and its reason
are called out explicitly. The default answer for "should this be
a new provider module" is: no, reuse `OpenAIProvider` via base-URL
+ catalog entry. New modules are only for new `ApiKind` variants.

## Relationship to plan 10 (extension system)

Plan 10 (pi-shaped extension system, not yet started) will let
extensions register providers at runtime. Every provider added
here stays in the built-in set — extensions will layer on top,
not replace the built-ins. No plan in this folder is invalidated
by plan 10.
