# Pi provider-set comparison

Reference check of this folder's plans against pi's actual
provider implementations at
`/home/daniel/Projects/agents/pi/packages/ai/src/providers/`.
pi is a mature multi-provider harness that's been shipping for
months, so treating its choices as the default behavior and
asking "why would we diverge" surfaces real gaps.

## Pi's shipping provider set

| pi `Api` value | pi file | anie mapping |
|---|---|---|
| `anthropic-messages` | `anthropic.ts` | `ApiKind::AnthropicMessages` ✅ shipped |
| `openai-completions` | `openai-completions.ts` | `ApiKind::OpenAICompletions` ✅ shipped |
| `openai-responses` | `openai-responses.ts` | `ApiKind::OpenAIResponses` (stubbed; plan 04) |
| `openai-codex-responses` | `openai-codex-responses.ts` | not planned — ChatGPT OAuth follow-up |
| `azure-openai-responses` | `azure-openai-responses.ts` | plan 05 (starts with Chat Completions) |
| `google-generative-ai` | `google.ts` | `ApiKind::GoogleGenerativeAI` (stubbed; plan 03) |
| `google-gemini-cli` | `google-gemini-cli.ts` | not planned — Google OAuth follow-up |
| `google-vertex` | `google-vertex.ts` | not planned — Vertex enterprise follow-up |
| `mistral-conversations` | `mistral.ts` | plan 02 ships via OpenAI-compat instead (native is a follow-up) |
| `bedrock-converse-stream` | `amazon-bedrock.ts` | plan 06 |

Plus OpenRouter, which pi routes through `openai-completions`
with a compat-flag-driven thinking format — no dedicated `Api`
value. Plan 01 follows the same approach.

## Where we match pi exactly

1. **OpenRouter via OpenAI-compat.** Both pi and our plan 01
   reuse the Chat Completions path and bake the aggregator
   quirks into the model catalog entries, not a separate
   provider.
2. **Bedrock gets its own module and ApiKind.** SigV4 + Event
   Stream framing is distinct enough that sharing code isn't
   worth it.
3. **Responses API as a distinct stream state machine.** Both
   treat it as a separate module from Chat Completions.
4. **Model-level capability flags drive behavior.** Pi's
   `OpenAICompletionsCompat` carries `thinkingFormat`,
   `supportsReasoningEffort`, `requiresThinkingAsText`, etc.
   per model; our `ReasoningCapabilities` + `ReplayCapabilities`
   serve the same purpose with narrower initial scope.

## Where we deliberately diverge

| Topic | Pi | anie plan | Reason |
|---|---|---|---|
| Mistral wire | Native SDK (`mistral-conversations`) | OpenAI-compat v1, native follow-up | v1 value/effort ratio: basic Mistral models work fine via OpenAI-compat. Native needed only for `magistral` reasoning. |
| Azure OpenAI | Responses only | Chat Completions first, Responses once plan 04 lands | Chat Completions covers Azure's existing GPT-4/GPT-4o fleet; Responses-only would gate all Azure use on plan 04. |
| Google SDK | `@google/genai` TypeScript SDK | Hand-rolled HTTP | No maintained Rust SDK exists. Quarterly contract re-audit is the cost. |
| ChatGPT Codex OAuth | `openai-codex-responses` module | Not in this folder | Belongs with the broader OAuth / subscription story (plan `provider_expansion_and_auth.md` §2). |
| Google OAuth (gemini-cli, vertex) | Two dedicated modules | Not in this folder | Same reason as above. |

## Pi findings that updated our plans

### OpenRouter's nested `reasoning.effort` (plan 01)

Pi's `openai-completions.ts:429` branches on
`compat.thinkingFormat === "openrouter"` and sends a nested
`reasoning: { effort: "high" }` object instead of the flat
`reasoning_effort: "high"` field. Without this branching,
reasoning requests against OpenRouter reasoning models silently
no-op.

Plan 01 originally missed this. Now calls out the nested shape
explicitly and introduces `ThinkingRequestMode::NestedReasoning`.

### OpenRouter provider routing preferences (plan 01)

Pi models the full routing-preferences object
(`types.ts:307-360`: `allow_fallbacks`, `zdr`, `order`, `only`,
`ignore`, `quantizations`, `sort`, `max_price`,
`preferred_min_throughput`, …) on the model compat blob. Plan 01
now includes a per-model `openrouter_routing` field with config-
level support, UI surface deferred.

### OpenRouter leaderboard headers — **not shipped in pi** (plan 01)

Original plan 01 recommended setting `HTTP-Referer` and
`X-Title` headers. Pi doesn't — grep turns up zero hits. They're
cosmetic for OpenRouter's public leaderboard, not functional.
Dropped from plan 01's v1 scope.

### Gemini `thoughtSignature` replay (plan 03)

Pi's `google-shared.ts:17` documents:

> `thoughtSignature` is an encrypted representation of the
> model's internal thought process; `thoughtSignature` can
> appear on ANY part type (text, functionCall, etc.).

Original plan 03 had
`requires_thinking_signature: false` for Gemini. Wrong —
reasoning Gemini models emit `thoughtSignature` and lose
reasoning continuity without it on turn 2. Plan 03 updated to
capture and replay, with per-block signature storage.

Plan 03 also incorporates pi's first-wins retention rule
(`google-shared.ts:40`, `retainThoughtSignature`) — some
streaming backends emit the signature only on the first delta
of a block and omit it on subsequent deltas; our state machine
must not overwrite a captured signature with an empty later
delta.

### Mistral magistral reasoning (plan 02)

Pi gives Mistral its own module precisely because magistral
reasoning doesn't surface through OpenAI-compat. Plan 02
originally treated Mistral as generic OpenAI-compat. Updated to
explicitly exclude magistral models from v1 and flag "native
Mistral provider" as a follow-up.

### Bedrock bearer-token auth (plan 06)

Pi supports `AWS_BEARER_TOKEN_BEDROCK` which bypasses SigV4.
Plan 06 originally required the full AWS SDK dependency
unconditionally; now splits into two feature flags (`bedrock`
for bearer-token-only, `bedrock-sigv4` for the full chain) so
users with bearer tokens get a slim binary.

## Cross-cutting observation: compat flags at the model level

Pi consolidates provider quirks into per-model compat blobs
(`OpenAICompletionsCompat`, `OpenAIResponsesCompat`, etc.). That
makes one Chat Completions provider handle OpenAI, OpenRouter,
xAI, Groq, Cerebras, local servers, zai, qwen, qwen-chat-template,
Vercel AI Gateway, and more — all by flipping flags.

Our architecture already moves this direction:
`ReasoningCapabilities` and `ReplayCapabilities` are per-model.
For plan 01 specifically we should extend (not replace) this
pattern with a minimal `OpenAICompletionsCompat` on `Model`:

- `thinking_request_mode: ThinkingRequestMode` (already exists,
  extend with `NestedReasoning` for OpenRouter).
- `openrouter_routing: Option<OpenRouterRouting>`.

Every subsequent OpenAI-compat provider ships as a catalog entry
+ a compat flip, matching pi's architectural precedent.

## What we're not adopting from pi

1. **Auto-generated model catalog.** Pi runs a nightly script
   (`scripts/generate-models.ts`) that pulls from OpenRouter's
   `/models` endpoint, Anthropic's pricing pages, etc., and
   writes a 500+ model `models.generated.ts`. Useful but
   expensive to port. anie's catalog stays curated for v1; live
   discovery covers the gap for users who need a specific
   model.
2. **`faux` fake provider** for deterministic tests. We have
   `MockProvider` already; scope overlap is enough to skip.
3. **Web UI components.** Out of scope for anie entirely.

## How to use this document

Read it before starting a provider plan to understand what's
already been decided and why. When updating a plan's content,
update the corresponding section here. If a new pi finding
applies to a plan, note it in the per-plan file first, then
summarize the divergence here.
