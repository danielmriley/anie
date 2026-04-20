# Plan 03 — Google Gemini

First provider in this folder that introduces a new `ApiKind`.
The enum already has `ApiKind::GoogleGenerativeAI` declared —
this plan writes the provider module behind it.

## User value

- **Free tier.** Gemini Flash has a generous free quota at
  <https://ai.google.dev>, making it the lowest-friction non-local
  option for new users.
- **1M-token context.** Gemini Pro's 1M / 2M context handles
  large codebases or document sets without compaction pressure.
- **Strong multimodal.** Gemini's native image / video / audio
  handling is a genuine differentiator. This plan ships only
  text + images; audio/video are follow-up work.

## Wire protocol

**New provider module: `anie-providers-builtin::gemini`**, behind
the existing stubbed `ApiKind::GoogleGenerativeAI`.

Protocol summary (Google Generative Language / AI Studio API):

- Endpoint:
  `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent`
- HTTP: POST with `Content-Type: application/json`, auth via
  `?key={api_key}` query param or `x-goog-api-key` header.
- Request shape: `{ contents: [ { role, parts: [...] } ], generationConfig: {...}, tools: [...] }`.
- Response: server-sent events, one JSON payload per event, not
  the usual `data: {...}\n\n` framing — just raw JSON objects
  separated by newlines (needs a distinct line-based SSE parser
  from the one OpenAI/Anthropic share).

## Auth shape

- **API key** via `?key={key}` (Google AI Studio path, the common
  consumer/dev one).
- Environment variable: `GEMINI_API_KEY` or `GOOGLE_API_KEY`
  (check both; `GEMINI_API_KEY` wins).
- API key URL: <https://aistudio.google.com/apikey>.

**Google Vertex AI** (enterprise-tier, requires ADC / service
account) is a separate ApiKind and separate plan. This plan
targets **AI Studio** only.

## Model catalog entries

| Model ID | Display | Context | Max out | Reasoning | Multimodal |
|---|---|---|---|---|---|
| `gemini-2.0-flash` | Gemini 2.0 Flash | 1M | 8k | none | text + images |
| `gemini-2.0-flash-thinking-exp` | Gemini 2.0 Flash Thinking | 1M | 64k | native (thinking_budget) | text |
| `gemini-2.5-pro-exp` | Gemini 2.5 Pro (exp) | 2M | 64k | native | text + images |

`cost_per_million` pulled from <https://ai.google.dev/pricing>
at implementation time, dated in the catalog comment.

## Provider module contract

Per the round-trip contract block convention from the
`adding-providers` skill, the top of `gemini.rs` declares:

```
| Field              | Source event          | Landing spot                        |
|--------------------|-----------------------|-------------------------------------|
| `candidates[].content.parts[].text` | `text_delta` | `ContentBlock::Text`           |
| `candidates[].content.parts[].functionCall` | `tool_use` start | `ContentBlock::ToolCall` |
| `candidates[].content.parts[].thought` | `thinking_delta` | `ContentBlock::Thinking`      |
| `candidates[].finishReason` | `message_stop` | `AssistantMessage::stop_reason`    |
| `usageMetadata.*`  | accumulated each event | `AssistantMessage::usage`          |
```

Intentionally dropped on replay (for this initial landing):

- `candidates[].safetyRatings` — Google's safety scores. Not
  needed for replay; can surface via `ToolResult.details` in a
  follow-up if we decide to render them.
- `candidates[].citationMetadata` — citations. Needed for RAG
  scenarios but out of scope here; mark `Intentionally dropped`
  with a comment so a future plan can wire them.
- `usageMetadata.cachedContentTokenCount` — shown in status bar
  once we wire it; for v1 roll into `cache_read_tokens` in
  `Usage`.

## Replay capabilities

Gemini's native thinking (`thinking_budget` + `thought=true` on
content parts) does **not** require a replay signature —
Gemini's turn 2 accepts the thought blocks without opaque tokens
the way Anthropic demands. Catalog entry:

```rust
replay_capabilities: Some(ReplayCapabilities {
    requires_thinking_signature: false,
    supports_redacted_thinking: false,
    supports_encrypted_reasoning: false,
}),
```

Keeping the field explicit rather than `None` documents the
decision.

## Implementation phases

This is larger than a batch plan — split into three phases to
keep each PR reviewable.

### Phase A — Provider module skeleton

- Create `crates/anie-providers-builtin/src/gemini/mod.rs`.
- Implement `GeminiProvider` with `convert_messages`,
  `build_request_body`, and an empty `process_event` skeleton.
- Unit tests for request-body shape (one turn, with and without
  images) matching fixtures captured from the real API.
- Register provider in `init_provider_registry`. Catalog entry
  for `gemini-2.0-flash` only.

### Phase B — Streaming parser + thinking

- Write the line-delimited JSON parser (distinct from the
  shared SSE parser; see `sse.rs`).
- Implement `process_event` covering text, function calls, and
  thinking parts.
- Add two more catalog entries
  (`gemini-2.0-flash-thinking-exp`, `gemini-2.5-pro-exp`).
- Invariant-suite integration (`provider_replay.rs`).

### Phase C — Multimodal images

- `ContentBlock::Image` handling in `convert_messages` — map
  anie's image block (if it exists; add if not) to Gemini's
  `{ inlineData: { mimeType, data } }` part shape.
- Test: fixture request with one image attachment, assert wire
  body is correct.
- Manual smoke against the real API.

## Auth plumbing

Gemini's key-in-query-param is unusual. Two options:

1. Handle it inside `GeminiProvider::build_request` as a URL
   suffix; `ResolvedRequestOptions::api_key` flows into the URL
   at send time.
2. Use the `x-goog-api-key` header instead (Google supports
   both). Cleaner — matches the header-based pattern the
   provider infrastructure already assumes.

**Pick option 2.** Less surface area changed.

## Test plan

Per phase:

### Phase A
| # | Test |
|---|---|
| 1 | `gemini_request_body_has_correct_contents_shape` — snapshot test. |
| 2 | `gemini_request_uses_x_goog_api_key_header` |
| 3 | `gemini_preset_registered` |

### Phase B
| # | Test |
|---|---|
| 4 | `gemini_streaming_text_delta_to_content_block` — fixture-driven. |
| 5 | `gemini_streaming_thought_part_to_thinking_block` |
| 6 | `gemini_function_call_part_to_tool_call` |
| 7 | `gemini_finish_reason_maps_to_stop_reason` |
| 8 | Invariant suite: `gemini_model()` and
   `build_gemini_body()` helpers added, all cross-provider
   invariants pass. |

### Phase C
| # | Test |
|---|---|
| 9 | `gemini_image_attachment_roundtrips_to_inline_data` |
| 10 | Manual smoke: two-turn conversation with an image attachment. |

## Exit criteria

- [ ] `ApiKind::GoogleGenerativeAI` has a real provider behind it.
- [ ] Three Gemini catalog entries, all appearing in
      `/providers` category picker under `Frontier`.
- [ ] Streaming parser handles text + thought + function-call
      parts.
- [ ] Invariant suite covers Gemini on every cross-provider
      invariant.
- [ ] Manual two-turn smoke with Gemini 2.5 Pro documented.

## Out of scope

- Vertex AI ADC / service-account auth (separate plan; different
  `ApiKind` and different base URL).
- Live API (WebSocket / bidirectional streaming) — Gemini's
  real-time audio/video path.
- Grounding / Google Search tool — Gemini's server-side tool
  that we don't implement yet.
- Safety-ratings UI surfacing.

## Dependencies

- Plan 00 (provider selection UX) — prerequisite.
- No dependency on plans 01 / 02 — this is a separate
  `ApiKind` and doesn't share their code path.
