# Plan 04 — OpenAI Responses API

Native path for OpenAI's o1 / o3 / Codex reasoning models. The
`ApiKind::OpenAIResponses` variant is already in the enum (stubbed
in anticipation of this). This plan writes the provider module
and extends `ReplayCapabilities` to cover encrypted reasoning.

## User value

- **First-class reasoning on o-series.** Chat Completions exposes
  o4-mini with `reasoning_effort`, but Responses is the
  first-class API: encrypted reasoning roundtrips across turns
  properly, and the parameter surface (`reasoning.effort`,
  `reasoning.summary`) is richer.
- **Codex-style code-generation.** OpenAI's codex variants ship
  on Responses; Chat Completions gets them later, if at all.
- **Future-proof.** OpenAI is steering new features through
  Responses. Keeping pace is cheaper than chasing features back
  into the Chat Completions provider one at a time.

## Wire protocol

**New provider module: `anie-providers-builtin::openai_responses`**.
Shares utility code with the existing
`anie-providers-builtin::openai` module (request building,
common OpenAI auth) via pulled-out helpers — not via
subclassing. New streaming parser because the event shapes are
different.

Endpoint: `https://api.openai.com/v1/responses`.

Key differences from Chat Completions:

| Aspect | Chat Completions | Responses |
|---|---|---|
| Request root | `{ messages, model, ... }` | `{ input, model, reasoning, ... }` |
| Messages | `messages: [{role, content, ...}]` | `input: [{type, role, content: [...]}]` |
| Tool use | `tools: [...]`, `tool_calls` in assistant msg | `tools: [...]`, `tool_use` items in input |
| Reasoning | `reasoning_effort: "high"` (flat) | `reasoning: { effort, summary }` object |
| Events | `data: {...}` chunks | Typed events (`response.created`, `response.output_text.delta`, `response.output_item.done`, …) |
| Opaque state | — | **`encrypted_content`** on reasoning items; required on replay |

## Auth shape

Same as existing OpenAI: `Authorization: Bearer {key}`, env
`OPENAI_API_KEY`. The auth-resolver chain needs no new code.

This plan uses the **same provider name** (`openai`) as the
existing Chat Completions provider. Users already configured for
OpenAI get access to Responses models automatically once catalog
entries land. Differentiation is at the `ApiKind` level; the
registry maps `ApiKind::OpenAIResponses` to the new provider
module.

## `ReplayCapabilities` extension

OpenAI Responses mints `encrypted_content` on reasoning items
that must be echoed back on turn 2+ for the model to maintain
reasoning continuity. This is the encrypted analog to Anthropic's
`thinking.signature`.

The existing `ReplayCapabilities` struct already has a
`supports_encrypted_reasoning: bool` field (added speculatively
in plan 03c of api_integrity_plans). This plan activates it:

1. `ContentBlock::Thinking` grows an optional `encrypted_content`
   field.
2. Serializer skips it when the target provider's
   `supports_encrypted_reasoning` is false (e.g., Chat
   Completions — the OpenAI catalog entry that declares
   `ApiKind::OpenAICompletions`).
3. Sanitizer drops Thinking blocks lacking
   `encrypted_content` when the target model *requires* it,
   matching the established signature-sanitization pattern.

Add a new `ContentBlock::Thinking` field only if one doesn't
already cover encrypted payloads; inspect
`crates/anie-protocol/src/content.rs` during implementation.

## Model catalog entries

| Model ID | Display | Context | Max out | Reasoning | Replay |
|---|---|---|---|---|---|
| `o3` | o3 | 200k | 100k | native (encrypted) | encrypted_reasoning |
| `o3-mini` | o3-mini | 200k | 100k | native (encrypted) | encrypted_reasoning |
| `o1` | o1 | 200k | 100k | native (encrypted) | encrypted_reasoning |
| `gpt-5` (*) | GPT-5 | 400k | 128k | native (encrypted) | encrypted_reasoning |

(*) Catalog entry placeholder — include only if `gpt-5` is
generally available at implementation time. Today's o3 / o1 are
the proven targets.

## Round-trip contract

```
| Field                                 | Source event                       | Landing spot                           |
|---------------------------------------|------------------------------------|----------------------------------------|
| `response.output_text.delta.text`    | `response.output_text.delta`       | `ContentBlock::Text`                   |
| `response.output_item.done(type=reasoning).content[*]`
                                       | `response.output_item.done`        | `ContentBlock::Thinking { thinking }` |
| `response.output_item.done(type=reasoning).encrypted_content`
                                       | `response.output_item.done`        | `ContentBlock::Thinking.encrypted_content` |
| `response.output_item.done(type=tool_use).*`
                                       | `response.output_item.done`        | `ContentBlock::ToolCall`               |
| `response.completed.response.stop_reason` | `response.completed`          | `AssistantMessage::stop_reason`        |
| `response.completed.response.usage.*`| `response.completed`               | `AssistantMessage::usage`              |
```

Intentionally dropped on replay:

- `reasoning.summary` — human-readable summary that's part of
  the output_item but isn't required on replay. Capture in
  logs / display; don't echo on turn 2.
- `output_item.status` — "completed" / "in_progress" lifecycle
  state. Not relevant after the stream terminates.

## Implementation phases

### Phase A — Shared OpenAI helpers

Factor out from `openai/mod.rs` into a new `openai_common/` or
`openai::shared` module:

- `authorization_header()` helper
- Common error classification for OpenAI-family 4xx/5xx
- Base URL canonicalization (the one that strips trailing
  slashes)

Purely a refactor; both existing tests and new Responses tests
rely on it.

### Phase B — Responses provider skeleton + non-streaming

- New module `openai_responses::mod.rs`.
- `convert_messages` (anie `Message` → Responses `input` items).
- `build_request_body` with `reasoning: { effort, summary }`
  block when the catalog says so.
- Stub `process_event` — just parses `response.completed` for
  the initial landing, enough to run a one-turn conversation
  buffered (no streaming display yet).
- Register in `init_provider_registry` behind
  `ApiKind::OpenAIResponses`.

### Phase C — Streaming parser

- Full event coverage: `response.output_text.delta`,
  `response.output_item.added`, `response.output_item.done`,
  `response.output_tool_call.delta`, `response.completed`,
  `error`.
- Top-of-file round-trip contract block from the table above.
- Invariant suite integration.

### Phase D — Encrypted reasoning roundtrip

- Extend `ContentBlock::Thinking` with
  `encrypted_content: Option<String>` (if not already there).
- Serializer path for `OpenAIResponses`: include
  `encrypted_content` on reasoning items when replaying
  assistant messages.
- Sanitizer: drop Thinking blocks without `encrypted_content`
  when target model requires it (mirror the Anthropic
  signature-required path).
- Integration test: two-turn conversation, assert the second
  request's input includes the encrypted_content from turn 1's
  assistant reasoning.

### Phase E — Error classifier + catalog

- Port the OpenAI Chat Completions error classifier to cover
  any Responses-specific 400 shapes (check for distinct error
  codes).
- Add `o3`, `o3-mini`, `o1` catalog entries.
- Bump `CURRENT_SESSION_SCHEMA_VERSION` if the
  `encrypted_content` field addition to `ContentBlock::Thinking`
  warranted it (it does; this is a schema change per plan
  05 api_integrity).

## Test plan

### Phase A
| # | Test |
|---|---|
| 1 | `openai_shared_base_url_canonicalization_strips_trailing_slash` (migrated) |
| 2 | existing Chat Completions tests still pass after refactor |

### Phase B
| # | Test |
|---|---|
| 3 | `responses_request_body_is_input_typed_not_messages` |
| 4 | `responses_reasoning_block_present_when_effort_configured` |

### Phase C
| # | Test |
|---|---|
| 5 | `responses_stream_text_delta_accumulates_to_content_block` |
| 6 | `responses_stream_output_item_done_emits_thinking_on_reasoning_type` |
| 7 | `responses_stream_tool_call_roundtrips` |
| 8 | `responses_completed_event_populates_usage` |
| 9 | Invariant suite covers Responses. |

### Phase D
| # | Test |
|---|---|
| 10 | `encrypted_content_present_on_turn_two_replay` — fixture-driven two-turn assert. |
| 11 | `thinking_block_without_encrypted_content_is_dropped_for_responses_model` |
| 12 | `schema_version_bumped_and_migration_note_added` |

### Phase E
| # | Test |
|---|---|
| 13 | `o3_and_o3_mini_catalog_entries_present_with_encrypted_reasoning_capability` |
| 14 | Manual two-turn smoke against real o3 API. |

## Exit criteria

- [ ] `ApiKind::OpenAIResponses` has a real provider behind it.
- [ ] At least three catalog entries (o3, o3-mini, o1).
- [ ] `ContentBlock::Thinking.encrypted_content` roundtrips
      across session save → load → replay.
- [ ] Two-turn conversation preserves reasoning continuity.
- [ ] Session schema version bumped; migration test passes.
- [ ] Invariant suite covers Responses on every cross-provider
      invariant.

## Out of scope

- Structured output (`response_format: json_schema`) — deferred;
  the existing `tool_calls` path covers most use cases.
- File-search / web-search tools (server-side OpenAI tools with
  no anie equivalent today).
- Multi-turn tool-use streaming with partial tool results — same
  UX gap that exists for Chat Completions too.
- **Codex Responses via ChatGPT OAuth.** Pi has a dedicated
  `openai-codex-responses` provider
  (`packages/ai/src/providers/openai-codex-responses.ts`) that
  talks to `chatgpt.com/backend-api` using an OAuth token from
  ChatGPT Plus/Pro. This plan ships standard Responses via
  API key only. A follow-up plan can add Codex OAuth after
  our broader OAuth story lands (tracked under
  [`docs/notes/provider_expansion_and_auth.md`](../notes/provider_expansion_and_auth.md)
  §2 "OAuth / subscription support").

## Dependencies

- Plan 00 (provider selection UX).
- Required reading: `docs/completed/api_integrity_plans/03c_replay_capabilities.md`
  (capabilities on Model) and
  `docs/completed/api_integrity_plans/05_session_schema_migration.md`
  (how to bump the schema cleanly). This plan does both.
