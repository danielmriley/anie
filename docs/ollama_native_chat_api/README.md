# Ollama native `/api/chat` codepath

**Add a first-class `OllamaChatApi` provider impl that talks to
Ollama's native `/api/chat` endpoint. Same trait as the other
providers; separate wire format. Unlocks two things the
OpenAI-compat layer cannot do: honoring `ThinkingLevel::Off` on
a thinking-capable model, and honoring the model's discovered
`context_length` on the wire via `num_ctx`.**

## Context

This plan is the implementation of the first item in the
"Deferred" section of
[`docs/ollama_capability_discovery/README.md:880-…`](../ollama_capability_discovery/README.md).
PRs 1–5 of that plan fixed the substring-heuristic false
positives and piped `/api/show` capabilities into
`Model.reasoning_capabilities`. They did not — and could not —
honor an explicit `Off` on a thinking-capable Ollama model, and
they did not honor discovered context lengths on the wire. Both
gaps are rooted in the same observation, verified empirically in
the parent plan at lines 492–524:

- `POST /v1/chat/completions` with `enable_thinking: false`
  (top-level or `chat_template_kwargs.enable_thinking: false`):
  Ollama silently ignores the field. The model still streams a
  `reasoning` field alongside content.
- `POST /v1/chat/completions` with `options.num_ctx: 16384`:
  silently ignored. `/api/ps` reports `context_length: 4096`
  (Ollama's default) regardless.
- `POST /api/chat` with `think: false`: actually disables
  thinking.
- `POST /api/chat` with `options.num_ctx: 16384`: actually
  applies the setting.

So the answer is not a tweak to the OpenAI-compat path — the
answer is a native `/api/chat` codepath. This plan ships it.

### The symptoms this closes

1. **Symptom 2 from the parent plan.** User sets thinking to
   `Off` on qwen3.5:9b (or any thinking-capable Ollama model).
   anie continues to render thinking blocks in the TUI because
   Ollama's OpenAI-compat layer ignored the disable signal and
   streamed reasoning anyway.
2. **Silent truncation on long-context Ollama models.** anie
   currently pins `Model.context_window` at the conservative
   32 k fallback for Ollama
   ([`model.rs:derive_ollama_reasoning_capabilities`'s sibling
   branch at `model.rs` in `to_model`](../../crates/anie-provider/src/model.rs),
   the explicit regression guard added in
   `ollama_caps/PR3`/`PR5`). If we ever propagated the
   discovered value (262 k for qwen3.5), compaction would grow
   conversations past Ollama's default 4 k
   `num_ctx` and the prompt would get silently truncated at
   the wire. We can only flip the guard once `num_ctx` is set
   per-request.

Both symptoms share one fix: talk to `/api/chat` and put
`think` and `options.num_ctx` on the wire ourselves.

## What pi-mono and codex do

Before designing further, the ollama_capability_discovery plan
surveyed two reference implementations. Summarizing what's
relevant here:

### pi-mono — stays on the OpenAI-compat path

pi-mono does not have a native-`/api/chat` codepath. It uses
Ollama's `/v1/chat/completions` only and therefore cannot
honor `ThinkingLevel::Off` on thinking-capable Qwen3+ models
served via Ollama. It *can* honor it on vLLM / SGLang via the
`qwen` / `qwen-chat-template` thinking-format variants we
adopted in `ollama_caps/PR4`, but Ollama itself is stuck.

Takeaway: there is no pi-shape to match for this specific
feature. This is an anie-specific extension, to be flagged
inline per CLAUDE.md §3.

### codex-rs — separate `ollama/` crate

codex (`openai/codex`, Rust workspace) ships a dedicated
`codex-rs/ollama/` crate that implements the native
`/api/chat` flow end-to-end. Key structural choices worth
borrowing:

- Ollama is its own Provider impl with its own streaming state
  machine. It does not attempt to share the OpenAI state
  machine, because the wire shape diverges materially (NDJSON
  vs SSE, tool-calls emitted as a single JSON object vs streamed
  chunks, a `thinking` field that appears alongside `content`
  rather than as a separate event channel).
- Server-version gating is done at discovery time, not at
  request time. The discovery layer tags each model with its
  "does this Ollama support `/api/chat` + `think`?" answer, and
  the runtime just reads the tag.
- Tool call arguments come as a parsed JSON object on the
  wire, not a streamed string. Codex normalises them into the
  same `ToolCall` shape everything else produces.

We adopt all three of these structural choices.

### Not adopted

- Codex's separate crate lives outside the `codex-rs/providers/`
  tree. anie's existing split (`anie-providers-builtin/src/{anthropic,openai,openrouter}`)
  is per-module, not per-crate. We add `anie-providers-builtin/src/ollama_chat/`
  as a new sibling module, matching the in-tree convention.
- Codex uses a handcrafted SSE-like framing adapter. Ollama
  actually emits plain NDJSON over a chunked HTTP response — we
  use `reqwest` `bytes_stream()` with a simple line splitter.

## The authoritative wire shape

**Important:** every shape below is stated from the Ollama docs
and the ollama_capability_discovery empirical curl session. The
implementer of PR 3 must re-verify each one with a live probe
before the streaming parser lands — previous plans on this
project have been wrong on provider-wire details when
written from memory. Specifically call `curl` against a local
Ollama instance and inspect the NDJSON before committing the
parser code. See "Empirical verification checklist" under PR 3.

### Request

`POST /api/chat` with JSON body:

```json
{
  "model": "qwen3:32b",
  "messages": [
    {"role": "system", "content": "…system prompt…"},
    {"role": "user",   "content": "hi"},
    {"role": "assistant", "tool_calls": [{"function": {"name": "bash", "arguments": {"command": "ls"}}}]},
    {"role": "tool", "content": "…tool result…"}
  ],
  "stream": true,
  "think": false,
  "tools": [ { "type": "function", "function": { "name": "bash", "description": "…", "parameters": {…} } } ],
  "options": {
    "num_ctx": 32768
  }
}
```

Fields:

- `model` — the id (ollama tag), e.g. `qwen3:32b`.
- `messages` — role/content pairs. Roles accepted:
  `system` · `user` · `assistant` · `tool`.
- `stream` — `true` to get NDJSON streaming.
- `think` — **only honored on thinking-capable models.** Maps
  directly from `ThinkingLevel`: `Off → false`, any non-Off
  level → `true`. Non-thinking models **must not** receive the
  field at all — they 400 (`"…does not support thinking"`)
  even on `think: false`.
- `tools` — OpenAI-shaped function-call schemas. Ollama
  accepts the same shape anie already builds for OpenAI.
- `options.num_ctx` — the context window for this request. This
  is the *only* wire-path to honor the discovered
  architectural context length. See "num_ctx semantics" below.

### Response — NDJSON, one JSON object per line

Text streaming (non-thinking model or `think: false`):

```
{"model":"qwen3:32b","created_at":"…","message":{"role":"assistant","content":"Hi"},"done":false}
{"model":"qwen3:32b","created_at":"…","message":{"role":"assistant","content":" there"},"done":false}
{"model":"qwen3:32b","created_at":"…","message":{"role":"assistant","content":"."},"done":false}
{"model":"qwen3:32b","created_at":"…","message":{"role":"assistant","content":""},"done":true,
 "done_reason":"stop","total_duration":…,"load_duration":…,"prompt_eval_count":123,
 "eval_count":4,"prompt_eval_duration":…,"eval_duration":…}
```

Thinking-enabled (`think: true`):

```
{"model":"…","message":{"role":"assistant","thinking":"Let me ","content":""},"done":false}
{"model":"…","message":{"role":"assistant","thinking":"consider…","content":""},"done":false}
{"model":"…","message":{"role":"assistant","thinking":"","content":"Here's the answer."},"done":false}
{"model":"…","message":{"role":"assistant","thinking":"","content":""},"done":true,
 "done_reason":"stop","prompt_eval_count":123,"eval_count":5}
```

Tool call (single message, arguments as a parsed object):

```
{"model":"…","message":{"role":"assistant","content":"","tool_calls":[
   {"function":{"name":"bash","arguments":{"command":"ls -la"}}}
 ]},"done":false}
{"model":"…","message":{"role":"assistant","content":""},"done":true,"done_reason":"tool_calls",
 "prompt_eval_count":123,"eval_count":8}
```

Notes:

- **Tool call arguments** arrive as a parsed JSON object, NOT a
  streamed string. anie's existing `ToolCall::arguments:
  serde_json::Value` is already the right shape — we serialize
  it directly, no string accumulation needed.
- **Tool call id / index**: Ollama does NOT emit an `id` on the
  tool call. We synthesize one (`toolu_<timestamp>_<index>`) at
  the state-machine layer to preserve the trait's invariant that
  every `ToolCall` has a non-empty id.
- **`done_reason` values observed** (to verify on PR 3):
  `stop` · `tool_calls` · `length` · `load` · others? PR 3 must
  probe and document each. The mapping to `StopReason` mirrors
  the OpenAI path: `length` → `ProviderError::ResponseTruncated`.
- **Usage**: `prompt_eval_count` → `input_tokens`,
  `eval_count` → `output_tokens`. `total_tokens` computed as
  the sum. This lets the compaction token estimator keep working
  unchanged — it reads `Usage::input_tokens` / `output_tokens` /
  `total_tokens` the same way regardless of provider.

### Error shape

Ollama errors come in two forms:

- **HTTP 4xx/5xx with a plain JSON body.** Example from the
  parent plan: `{"error":{"message":"think value \"low\" is
  not supported for this model","type":"api_error"}}`. The same
  classifier we added in `ollama_caps/PR2`
  (`looks_like_native_reasoning_compat_body` in
  [`reasoning_strategy.rs:201-220`](../../crates/anie-providers-builtin/src/openai/reasoning_strategy.rs))
  needs a sibling in the new module — but much simpler, because
  `/api/chat` has less retry-overlap surface. PR 6 addresses
  this.
- **HTTP 200 with `{error: "…"}` inline in the NDJSON stream**
  before `done:true`. Example: `{"error":"model \"foo\" not
  found, try pulling it first"}`. The streaming parser must
  treat this as a terminal ProviderError rather than a normal
  chunk.

### `num_ctx` semantics

From the parent plan (lines 359–367) and confirmed empirically:
changing `num_ctx` forces Ollama to reload the model into VRAM.
That means:

- We set `num_ctx = Model.context_window` on every request.
  Ollama's server keeps the same value cached per model so
  consecutive requests with the same `num_ctx` do not trigger
  a reload.
- If the user switches models mid-session, Ollama naturally
  unloads the old one. The reload cost is already paid in that
  case.
- If the user later overrides `num_ctx` via the `/context-length`
  slash command (see the sibling plan), a one-time reload
  happens on the next request. Documented.

## Design

### New `ApiKind` variant

Add `OllamaChatApi` to `anie_provider::ApiKind` at
[`api_kind.rs:5`](../../crates/anie-provider/src/api_kind.rs).
Additive; no serde rename needed (PascalCase consistent with
existing variants).

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ApiKind {
    AnthropicMessages,
    OpenAICompletions,
    OpenAIResponses,
    GoogleGenerativeAI,
    /// Ollama's native /api/chat endpoint. Preferred over
    /// OpenAICompletions for Ollama backends because only this
    /// path honors `think: false` and `options.num_ctx` on the
    /// wire.
    OllamaChatApi,
}
```

### Module layout

New `anie-providers-builtin/src/ollama_chat/` with the same
per-concern split that `openai/` uses:

```
src/ollama_chat/
  mod.rs           (OllamaChatProvider, Provider impl entry point)
  convert.rs       (LlmMessage ↔ Ollama wire messages; tool-schema passthrough)
  streaming.rs     (OllamaChatStreamState — NDJSON state machine)
  ndjson.rs        (newline-delimited JSON line splitter over bytes_stream())
```

`convert.rs`, `streaming.rs`, and `ndjson.rs` are each
pub(super). `mod.rs` exposes only `OllamaChatProvider`.

### Provider impl

```rust
#[derive(Clone)]
pub struct OllamaChatProvider {
    client: reqwest::Client,
}

impl OllamaChatProvider {
    pub fn new() -> Self { … }
}

impl Provider for OllamaChatProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> { … }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        // Same shape as OpenAI's convert_messages for now —
        // ollama accepts the OpenAI role vocabulary verbatim.
    }
}
```

Registered from
[`anie-providers-builtin/src/lib.rs:30-36`](../../crates/anie-providers-builtin/src/lib.rs):

```rust
pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register(ApiKind::AnthropicMessages, Box::new(AnthropicProvider::new()));
    registry.register(ApiKind::OpenAICompletions,   Box::new(OpenAIProvider::new()));
    registry.register(ApiKind::OllamaChatApi,       Box::new(OllamaChatProvider::new()));  // NEW
}
```

### Streaming state machine

`OllamaChatStreamState` mirrors `OpenAiStreamState` at
[`openai/streaming.rs:47-349`](../../crates/anie-providers-builtin/src/openai/streaming.rs).
Signature:

```rust
pub(super) struct OllamaChatStreamState {
    model_id: String,
    text: String,
    thinking: String,
    tool_calls: Vec<ToolCall>,
    usage: Usage,
    done_reason: Option<String>,
    finished: bool,
    tool_call_counter: u32,   // synthesizes tool-call ids
}

impl OllamaChatStreamState {
    pub(super) fn new(model: &Model) -> Self { … }

    /// Consume one NDJSON line.
    pub(super) fn process_line(
        &mut self,
        line: &str,
    ) -> Result<Vec<ProviderEvent>, ProviderError> { … }
}
```

Event emission rules (mirrors openai/streaming.rs semantics):

- `message.content` non-empty → `ProviderEvent::TextDelta(content)`.
- `message.thinking` non-empty → `ProviderEvent::ThinkingDelta(thinking)`.
- `message.tool_calls` present → for each entry: synthesize id,
  emit `ToolCallStart`, then — because arguments arrive whole —
  emit a single `ToolCallDelta` with the entire JSON
  serialisation of `arguments`, then emit `ToolCallEnd`. This
  keeps downstream consumers symmetric with the OpenAI path
  where deltas accumulate incrementally; for Ollama every tool
  call just happens to be "one big chunk".
- `done:true` → finalise: classify `done_reason`, emit
  `ProviderEvent::Done(AssistantMessage { … })`.
- Inline `{error: "…"}` line → terminal error, classified via
  `classify_ollama_error_body`.

Empty-content guard mirrors
[`openai/streaming.rs:278-281`](../../crates/anie-providers-builtin/src/openai/streaming.rs):
if the stream finishes with no text and no tool calls,
distinguish `done_reason == "length"` (route to
`ProviderError::ResponseTruncated`) from genuine
"only reasoning came back" (route to
`ProviderError::EmptyAssistantResponse`, already terminal in
`retry_policy.rs`).

### NDJSON splitter

Dedicated helper in `ndjson.rs`:

```rust
use futures::{Stream, StreamExt};

pub(super) struct NdjsonLines<S> { inner: S, buffer: String }

impl<S> Stream for NdjsonLines<S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<String, ProviderError>;
    // yields one complete line (without trailing \n) per poll.
}
```

This is the only code we write at the transport layer — it's
~60 LOC. Internally handles: partial lines across HTTP chunks,
`\r\n` vs `\n`, empty lines (ignored, not treated as `done`).

### Migration from `OpenAICompletions` for Ollama models

Two surfaces where a catalog entry can carry
`ApiKind::OpenAICompletions` for an Ollama endpoint today. (A
third surface — `~/.anie/state.json` — turns out not to need
migration. That file only stores
`provider: Option<String>` / `model: Option<String>` strings at
[`runtime_state.rs:13-22`](../../crates/anie-cli/src/runtime_state.rs)
and never persists `ApiKind` directly. Resolution against the
catalog at load time recovers whichever `ApiKind` discovery has
tagged the model with, so once this plan ships PR 5's tagging
change, state.json picks up the new routing automatically.
Plumbed for free.)

1. **Discovered models** — built by
   [`model_discovery::discover_ollama_tags`](../../crates/anie-providers-builtin/src/model_discovery.rs)
   and
   [`local::probe_openai_compatible`](../../crates/anie-providers-builtin/src/local.rs).
   PR 5 of this plan flips the output: both code paths tag
   Ollama models with `ApiKind::OllamaChatApi` instead.
2. **User-edited `config.toml`** — explicitly declared
   `api = "OpenAICompletions"` on a `[[providers.ollama.models]]`
   block. PR 5 leaves these alone by default (user-authored
   config is authoritative) and adds a warning log at load time:
   `"provider 'ollama' declared api = \"OpenAICompletions\"
   but targets an Ollama endpoint. Consider updating to
   api = \"OllamaChatApi\" to honor thinking-off and num_ctx."`

### `to_model` context-window propagation

In [`model.rs:to_model`](../../crates/anie-provider/src/model.rs),
the current Ollama regression guard pins `context_window` to
32 768. PR 6 of this plan flips the guard: **when `api ==
OllamaChatApi`**, propagate the discovered `context_length`
verbatim. Keep the 32 k fallback for legacy `OpenAICompletions`
Ollama entries so users who haven't migrated don't regress (the
OpenAI-compat layer still can't honor `num_ctx` on the wire).

```rust
let context_window = match (self.provider.eq_ignore_ascii_case("ollama"), api) {
    (true, ApiKind::OpenAICompletions) => 32_768,     // legacy guard
    _                                  => self.context_length.unwrap_or(32_768),
};
```

### Tool schema passthrough

[`openai/convert.rs`](../../crates/anie-providers-builtin/src/openai/convert.rs)
emits OpenAI-shape tool JSON. Ollama accepts the identical
shape — confirmed in the request example above. PR 3 can call
into the existing `convert_tools` helper rather than rebuilding
the schema.

### No streaming-options / stop-sequences plumbing

Scope is deliberately tight. This plan does not port:

- `stop` sequences
- `keep_alive` tuning
- `num_predict` (we send `options.num_ctx` only)
- `format: "json"` mode
- `/api/embed` or `/api/generate`

All documented in "Deferred" below.

## Files to touch

| File | PR | What |
|------|----|------|
| `crates/anie-provider/src/api_kind.rs` | 1 | Add `OllamaChatApi` variant |
| `crates/anie-provider/src/api_kind.rs` tests / `tests.rs` | 1 | Serde round-trip |
| `crates/anie-providers-builtin/src/ollama_chat/mod.rs` | 2 | Scaffold `OllamaChatProvider` returning `FeatureUnsupported` |
| `crates/anie-providers-builtin/src/lib.rs` | 2 | Register under `ApiKind::OllamaChatApi` |
| `crates/anie-providers-builtin/src/ollama_chat/ndjson.rs` | 3 | Line splitter over `bytes_stream()` |
| `crates/anie-providers-builtin/src/ollama_chat/convert.rs` | 3 | Request-body building (messages, stream flag) |
| `crates/anie-providers-builtin/src/ollama_chat/streaming.rs` | 3 | `OllamaChatStreamState::process_line`, text-only path |
| `crates/anie-providers-builtin/src/tool_schema.rs` | 4 | Extract `openai_function_schema(&[ToolDef]) -> Vec<Value>` from `openai/mod.rs:488-502` into a free helper |
| `crates/anie-providers-builtin/src/openai/mod.rs` | 4 | Replace `OpenAIProvider::convert_tools` body with a call to `tool_schema::openai_function_schema` |
| `crates/anie-providers-builtin/src/ollama_chat/convert.rs` | 4 | Add `think` field + tool serialisation via the new helper |
| `crates/anie-providers-builtin/src/ollama_chat/streaming.rs` | 4 | Thinking deltas, tool-call lifecycle, usage from `done:true` |
| `crates/anie-providers-builtin/src/model_discovery.rs` | 5 | Tag discovered Ollama models with `OllamaChatApi` |
| `crates/anie-providers-builtin/src/local.rs` | 5 | Same, for the probe path |
| `crates/anie-config/src/lib.rs` | 5 | Log a deprecation warning when a `[[providers.ollama.models]]` block declares `api = "OpenAICompletions"` |
| `crates/anie-provider/src/model.rs` (`to_model`) | 6 | Flip context-window regression guard for `OllamaChatApi` |
| `crates/anie-providers-builtin/src/ollama_chat/mod.rs` | 6 | Send `options.num_ctx = model.context_window` |

## Phased PRs

One small change per PR. Each PR self-contained, tests + clippy
green, mergeable independently.

### PR 1 — `ApiKind::OllamaChatApi` variant + serde round-trip

**Why first:** schema addition only. Lets every downstream PR
reference the variant. No behavior change anywhere.

**Scope:**

- Add the variant to
  [`api_kind.rs:5`](../../crates/anie-provider/src/api_kind.rs).
  PascalCase, no serde rename, additive.
- Add a serde round-trip test that the new variant survives
  `to_string` → `from_str` unchanged and round-trips through
  a `Model` serialized to TOML.
- Add a forward-compat test: an older-schema serialized `Model`
  with `api = "OpenAICompletions"` still loads.

**Tests:**

- `ollama_chat_api_variant_round_trips_serde_name`
- `forward_compat_old_openai_completions_models_still_load`

### PR 2 — Scaffold `OllamaChatProvider` placeholder

**Why second:** lets us register the provider in the registry
without breaking anything. Every call through it returns
`FeatureUnsupported` with a clear message until PR 3 ships the
real streaming path. This means an accidental catalog entry
with `api = "OllamaChatApi"` during development produces a
recognizable error instead of a compile failure or a silent
drop.

**Scope:**

- New `crates/anie-providers-builtin/src/ollama_chat/mod.rs`
  with the struct + `Provider` impl. `stream()` returns
  `Err(ProviderError::FeatureUnsupported("…OllamaChatApi not yet implemented…".into()))`.
- Wire into `register_builtin_providers` at
  [`lib.rs:30-36`](../../crates/anie-providers-builtin/src/lib.rs).
- `pub use ollama_chat::OllamaChatProvider;`

**Tests:**

- `registry_routes_ollama_chat_api_to_placeholder_until_pr3`
- `scaffold_returns_feature_unsupported_error_with_actionable_message`

### PR 3 — Native request body + NDJSON streaming, text-only

**Why third:** the biggest PR of the plan. Lands the actual
wire implementation, but intentionally scoped: text only,
happy-path, no thinking, no tool calls, no usage.

**Empirical verification checklist** (must be done before
writing the parser code):

```bash
# From a machine with a local Ollama running qwen3:32b:

# 1. Basic text streaming — confirm NDJSON framing and the
#    shape of the `message.content` field and the final
#    `done:true` line.
curl -s http://localhost:11434/api/chat \
  -d '{"model":"qwen3:32b","messages":[{"role":"user","content":"hi"}],
       "stream":true}' | head -20

# 2. `think` field absent on a non-thinking model — confirm
#    the expected 400 body:
curl -s http://localhost:11434/api/chat \
  -d '{"model":"gemma3:1b","messages":[{"role":"user","content":"hi"}],
       "stream":true,"think":false}'

# 3. `options.num_ctx` on the wire — confirm no server error
#    and check /api/ps that the loaded context_length changed:
curl -s http://localhost:11434/api/chat \
  -d '{"model":"qwen3:32b","messages":[{"role":"user","content":"hi"}],
       "stream":true,"options":{"num_ctx":16384}}' > /dev/null
curl -s http://localhost:11434/api/ps

# 4. Error body on unknown model:
curl -s http://localhost:11434/api/chat \
  -d '{"model":"nope:1b","messages":[{"role":"user","content":"hi"}],
       "stream":true}'

# 5. `done_reason` variants — run a few prompts and note every
#    value observed. Document them in streaming.rs.
```

Paste the raw outputs into the PR description so future
reviewers can diff against them. This is the same evidence-
first discipline CLAUDE.md §3 requires for pi comparisons.

**Scope:**

- `ollama_chat/ndjson.rs`: line splitter over `bytes_stream()`
  with partial-line buffering. Tests: multi-byte UTF-8
  boundary crossing chunks; a chunk containing multiple lines;
  a trailing incomplete line; `\r\n` and `\n` both accepted.
- `ollama_chat/convert.rs`: build request body (no thinking,
  no tools, no num_ctx yet). Tests: request body shape
  for a minimal `{system, user}` context; pass through of
  `serde_json::Value` content from `LlmMessage`.
- `ollama_chat/streaming.rs`: `OllamaChatStreamState` handling
  `message.content` deltas and the final `done:true` line.
  No thinking. No tool calls. `done_reason` mapped to StopReason
  via a simple match (known values documented from the curl
  probes above).
- `ollama_chat/mod.rs`: plumb everything together. `stream()`
  constructs the body via `convert`, posts, wraps the response
  in `ndjson::NdjsonLines`, drives `OllamaChatStreamState` per
  line, emits `ProviderEvent`s to the `ProviderStream`.
- Remove the placeholder `FeatureUnsupported` return from PR 2.

**Tests:**

- `request_body_contains_model_stream_and_messages_only`
- `ndjson_splitter_handles_chunks_split_across_boundaries`
- `ndjson_splitter_handles_utf8_across_chunk_boundaries`
- `streaming_state_emits_text_deltas_then_done`
- `streaming_state_routes_done_reason_length_to_response_truncated`
- `streaming_state_routes_inline_error_to_provider_error`
- `empty_assistant_response_surfaces_as_typed_error`

### PR 4 — Thinking + tool calls + usage

**Why fourth:** feature-complete the provider with the data
channels PR 3 deferred. Small, focused additions on top of the
PR 3 machinery.

**Scope:**

- New file `anie-providers-builtin/src/tool_schema.rs`:
  - Extract the 14-LOC function body from
    [`openai/mod.rs:488-502`](../../crates/anie-providers-builtin/src/openai/mod.rs)
    into a free function `pub(crate) fn openai_function_schema(tools: &[ToolDef]) -> Vec<Value>`.
    Takes a slice, no `&self`; the existing method doesn't use
    it.
  - Update `OpenAIProvider::convert_tools` to a one-line
    passthrough: `tool_schema::openai_function_schema(tools)`.
    Preserves the trait method so external callers are
    unchanged; the method body is just delegation now.
- `ollama_chat/convert.rs`:
  - Add `think: bool` to the body derived from
    `StreamOptions::thinking` (non-Off → `true`, Off →
    `false`). **Gate on the model's declared thinking
    capability** — for a Model with `reasoning_capabilities =
    None` (non-thinking per ollama_caps PR 5), omit the field
    entirely to avoid the 400.
  - Serialize `tools` via
    `tool_schema::openai_function_schema` — Ollama's `/api/chat`
    accepts the identical OpenAI tool schema, verified in the
    PR 3 empirical-probe session.
- `ollama_chat/streaming.rs`:
  - Extract `message.thinking` → `ThinkingDelta`.
  - Extract `message.tool_calls` as a single-shot event:
    synthesize id via `tool_call_counter`, emit `ToolCallStart`
    + `ToolCallDelta(serde_json::to_string(arguments))` +
    `ToolCallEnd`.
  - Parse usage from the final `done:true` line:
    `prompt_eval_count` → `input_tokens`,
    `eval_count` → `output_tokens`, sum → `total_tokens`.

**Tests:**

- `openai_function_schema_extraction_matches_prior_output`
  (regression: a pre-extraction snapshot of
  `convert_tools(sample)` matches the post-extraction
  `openai_function_schema(sample)` byte-for-byte).
- `request_body_includes_think_true_for_low_medium_high`
- `request_body_includes_think_false_for_off`
- `request_body_omits_think_field_for_non_thinking_capable_model`
  (invariant: gemma3:1b gets no `think` field regardless of
  `StreamOptions::thinking`).
- `streaming_state_emits_thinking_deltas_when_think_is_true`
- `streaming_state_emits_tool_call_lifecycle_for_arguments_object`
- `streaming_state_populates_usage_from_done_line`
- `tool_call_id_is_synthesized_when_ollama_omits_it`

### PR 5 — Discovery + probe tag Ollama as `OllamaChatApi`

**Why fifth:** now that `OllamaChatApi` *works*, flip the
discovery paths so new Ollama model records route through it
by default.

**Scope:**

- `model_discovery::discover_ollama_tags` (currently
  [`model_discovery.rs`](../../crates/anie-providers-builtin/src/model_discovery.rs)):
  set `ApiKind::OllamaChatApi` when building the discovered
  `Model` via `to_model`. Remove hard-coded
  `ApiKind::OpenAICompletions`.
- `local::probe_openai_compatible` at
  [`local.rs:probe_openai_compatible`](../../crates/anie-providers-builtin/src/local.rs):
  when `is_ollama_probe_target` (already exists from
  ollama_caps/PR5) returns true, construct the Model with
  `ApiKind::OllamaChatApi`. Non-Ollama OpenAI-compat servers
  (`lmstudio`, `vllm`, `unknown`) continue to use
  `OpenAICompletions`.
- Config-file warning in the `anie-config` load path: if a
  user-edited `[[providers.ollama.models]]` block declares
  `api = "OpenAICompletions"`, log (not print) a warning with
  the recommended upgrade.

No state.json migration: `RuntimeState` stores only provider +
model id strings, so `ApiKind` is recovered from the catalog on
every load and naturally follows PR 5's tagging change.

**Tests:**

- `ollama_discovery_tags_models_as_ollama_chat_api_after_pr5`
- `local_probe_tags_ollama_models_as_ollama_chat_api_after_pr5`
- `lmstudio_probe_still_uses_openai_completions`
- `config_toml_with_legacy_ollama_api_logs_warning_but_loads_unchanged`
- `state_json_with_legacy_provider_model_resolves_to_new_api_kind_via_catalog`
  (regression guard: a state.json written before this plan,
  with `provider = "ollama"` and `model = "qwen3:32b"`, loads
  fine and the resolved `Model` carries `OllamaChatApi` because
  the catalog tagged it that way).

### PR 6 — Honor discovered `context_length` + send `num_ctx` on the wire

**Why last:** the final mile of the Symptom-2 fix. Only safe to
land after PR 5: we need `OllamaChatApi` to be the default
routing path for Ollama so the old wire layer (which silently
drops `num_ctx`) can't receive the new context-window value.

**Scope:**

- [`model.rs:to_model`](../../crates/anie-provider/src/model.rs):
  replace the Ollama regression guard with an `api`-conditional
  one:

  ```rust
  let context_window = match (self.provider.eq_ignore_ascii_case("ollama"), api) {
      (true, ApiKind::OpenAICompletions) => 32_768,     // legacy, silent-drop path
      _                                  => self.context_length.unwrap_or(32_768),
  };
  ```

- `ollama_chat/convert.rs`: set
  `options.num_ctx = model.context_window` on every request.
  Document that Ollama only reloads when the value changes
  across consecutive requests.
- Update the comment at the PR 3/5 regression-guard tests
  in `model.rs` to reflect that the native-path case now
  propagates.

**Tests:**

- `to_model_propagates_ollama_context_length_under_ollama_chat_api`
- `to_model_retains_32k_guard_for_legacy_openai_completions_ollama`
- `ollama_chat_request_body_contains_num_ctx_equal_to_context_window`
- `ollama_chat_request_body_does_not_override_num_ctx_across_calls`
  (same model, two calls → both bodies have identical `num_ctx`
  so Ollama does not reload).

## Test plan

Per-PR tests as enumerated above. Cross-cutting:

| # | Test | Where |
|---|------|-------|
| Manual | qwen3.5:9b on local Ollama, thinking=Off. Verify NO `thinking` block appears in the TUI output. (Symptom-2 closure.) | smoke |
| Manual | qwen3:32b on local Ollama, thinking=Low/Medium/High. Verify reasoning renders AND the final answer renders; done_reason=`stop`. | smoke |
| Manual | qwen3:32b on local Ollama, prompt that triggers bash tool call. Verify tool call executes end-to-end; no stream-parsing errors. | smoke |
| Manual | qwen3:32b on local Ollama, 200-turn synthetic conversation. Verify context never truncates silently; compaction fires at the correct point given the discovered context length (262 k for qwen3.5; 32 k for qwen3:32b, whatever /api/show reports). | smoke |
| Manual | gemma3:1b on local Ollama, thinking=Off/Low/High. Verify NO 400s; the `think` field is silently dropped per PR 4's invariant. | smoke |
| Manual | Upgrade path: `~/.anie/state.json` previously written by an older anie (just `provider: "ollama"`, `model: "qwen3:32b"`). Start anie; verify the active model resolves to an `OllamaChatApi` catalog entry and a turn succeeds. | smoke |
| Manual | User-edited config.toml with `api = "OpenAICompletions"` + Ollama base_url. Start anie; verify a single deprecation warning in the log (`~/.anie/logs/anie.log`) and the OLD wire layer still runs. | smoke |
| Auto | `cargo test --workspace` green. | CI |
| Auto | `cargo clippy --workspace --all-targets -- -D warnings` clean. | CI |

## Risks

- **`/api/chat` server-version gating.** The endpoint has
  existed since Ollama 0.1.x and the `think` parameter since
  0.5.x. Pre-0.5 Ollama will 400 on `think:false`. Mitigated
  by ollama_caps/PR2's retry safety net catching the "not
  supported for this model"-shaped message; the retry drops
  reasoning fields and proceeds. Document in the PR 3 body.
  If pre-0.5 becomes a real support burden we add explicit
  version gating at discovery time (query `/api/version`); for
  now we rely on the runtime retry.

- **Tool-call shape variance across Ollama versions.** Tool
  calling on Ollama has been evolving. Older versions may emit
  tool calls as plain assistant text. PR 4's tool-call
  extraction must tolerate missing fields gracefully: no
  `tool_calls` key present → behave as text only; `arguments`
  missing or malformed → synthesize an empty object and log a
  warning (matches the OpenAI-compat path's defense at
  [`streaming.rs:302-310`](../../crates/anie-providers-builtin/src/openai/streaming.rs)).

- **NDJSON framing edge cases.** Very long single lines can
  split across multiple HTTP chunks. `ndjson.rs` must not
  truncate. PR 3's tests specifically cover this.

- **Schema drift on sessions.** Sessions opened with an
  `OpenAICompletions`-tagged Ollama model and then reloaded
  against a catalog that now tags it `OllamaChatApi` — the
  session's persisted provider/model id still resolves; the
  catalog's `api` determines the dispatch. Forward-compat test
  in PR 5 guards this.

- **User-intended downgrade via config.toml.** If a user
  deliberately sets `api = "OpenAICompletions"` on a
  `[[providers.ollama.models]]` block (e.g. to work around a
  `/api/chat`-specific bug), we honor it: config.toml stays
  authoritative, only the deprecation warning fires. No silent
  overrides.

- **Compaction token accounting.** The switch from
  `OpenAICompletions` to `OllamaChatApi` for Ollama models
  means `Usage` now comes from `prompt_eval_count` /
  `eval_count` instead of the OpenAI-compat `usage.prompt_tokens`
  / `completion_tokens`. The field wiring in PR 4 ensures
  these values land in the same `Usage` struct, but we
  should spot-check the compaction path reads total_tokens
  unchanged from the session log. Regression test in PR 6.

## Exit criteria

- [ ] PR 1 merged: `ApiKind::OllamaChatApi` variant exists and
      round-trips through serde.
- [ ] PR 2 merged: `OllamaChatProvider` registered; calls return
      `FeatureUnsupported`.
- [ ] PR 3 merged: text streaming works end-to-end against a
      real Ollama instance. Empirical curl probes documented in
      PR description.
- [ ] PR 4 merged: thinking + tool calls + usage all flow.
      Symptom-2 manual smoke test passes (thinking=Off →
      no thinking blocks rendered).
- [ ] PR 5 merged: new discoveries + probes tag Ollama models
      as `OllamaChatApi`. Legacy state.json resolves cleanly via
      the catalog without needing a migration step. User-
      authored config.toml `api = "OpenAICompletions"` on Ollama
      providers keeps working and logs a deprecation warning.
- [ ] PR 6 merged: `num_ctx` on the wire. `Model.context_window`
      reflects `/api/show` data. Long-context manual smoke test
      passes.
- [ ] Every Symptom-1 and Symptom-2 scenario in the parent plan
      completes without a 400 and without spurious thinking
      blocks.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] No unflagged anie-specific deviation from pi: every place
      we diverge (the entire OllamaChatApi plane is one such
      place) is commented with the rationale (per CLAUDE.md §3).

## Deferred

- **Other `/api/chat` options beyond `num_ctx`.** `num_predict`
  (max output tokens), `temperature`, `stop`, `keep_alive`,
  `format: "json"`. Each has an existing counterpart in the
  OpenAI-compat plumbing (`StreamOptions::temperature`,
  `options.max_tokens`); wiring them through is a follow-up
  plan once this codepath is stable.
- **`/api/generate` (legacy, non-chat).** Some Ollama users
  script against it. Out of scope.
- **`/api/embed`.** anie does not currently use embeddings;
  adding them requires a separate feature (retrieval / memory).
- **Ollama Cloud / remote Ollama instances.** The plan assumes
  localhost. Remote Ollama works identically on the wire;
  what's missing is auth-header plumbing in the probe path.
  Punted until a user asks.
- **Server-version probing.** Instead of relying on the retry
  safety net to handle pre-0.5 Ollama, we could `GET
  /api/version` at discovery time and refuse to tag a model as
  `OllamaChatApi` if the server is too old. Adds a network call
  and complexity; defer until we see real failures.
- **`/context-length` slash command for user overrides.** Its
  own plan at [`../ollama_context_length_override/README.md`](../ollama_context_length_override/README.md).

## Reference

### Ollama docs

- `/api/chat` reference:
  <https://github.com/ollama/ollama/blob/main/docs/api.md#generate-a-chat-completion>
- `/api/show` reference:
  <https://github.com/ollama/ollama/blob/main/docs/api.md#show-model-information>
- `think` parameter announcement: Ollama v0.5 release notes.

### anie sites

- Parent plan: `docs/ollama_capability_discovery/README.md` —
  Deferred section at line 880 points to this plan.
- pi comparison: `docs/anie_vs_pi_comparison.md` — see
  provider-wire divergences.
- Existing provider shape to mirror:
  `crates/anie-providers-builtin/src/openai/` (mod + streaming
  + convert + reasoning_strategy + tagged_reasoning).
- Existing state machine precedent:
  `crates/anie-providers-builtin/src/openai/streaming.rs:47-349`.

### codex-rs

- `codex-rs/ollama/` — separate crate implementing the native
  path. Reference only; we stay in-tree per anie's module
  convention.
