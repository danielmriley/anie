# Plan 04 — Provider HTTP + discovery unification

## Motivation

`crates/anie-providers-builtin/src/anthropic.rs` (687 LOC) and
`crates/anie-providers-builtin/src/openai.rs` (2084 LOC) duplicate:

- Auth / header construction (OpenAI: bearer_auth at ~lines 148–161;
  Anthropic: manual `x-api-key` at ~lines 108–119).
- HTTP status-code → `ProviderError` classification (both call
  `classify_http_error` but each wraps its own response flow).
- Custom-header loops over `model.custom_headers` (identical shape).
- Retry-around-send logic (slightly divergent).
- Tool-call state tracking, with one tracking `started`/`ended`
  booleans (OpenAI) and the other leaving `None` in a blocks map
  (Anthropic).

`crates/anie-providers-builtin/src/model_discovery.rs` (925 LOC) has
three parallel discovery functions:

- `discover_openai_compatible_models` (lines 220–267)
- `discover_anthropic_models` (lines 269–309)
- `discover_ollama_tags` (lines 311–365)

Each constructs a fresh `reqwest::Client`, sets up auth headers, and
handles errors. They already share `send_discovery_request` (line
368) but call it with enough duplicated setup that ~200 LOC of
per-function code could collapse into one dispatching function.

## Design principles

1. **One HTTP request builder per crate.** Shared by both
   providers. Owns headers, auth style, timeout, status
   classification.
2. **One model-discovery dispatch.** Keyed on `ApiKind`.
3. **Tool-call reassembly is shared shape.** Both providers use the
   same `ToolCallAssembler`.
4. **HTTP client is reused.** One `reqwest::Client` per crate, not
   per call.
5. **No behavior change.** Same wire output to providers, same
   provider-event stream back.

## Preconditions

Plan 01 (`openai.rs` module split) should land first. The extractions
here assume `openai/mod.rs` is the `Provider` impl facade and that
streaming/convert/reasoning_strategy are in their own modules. If
plan 01 has not landed, adjust the file table — the logic is the
same.

---

## Phase 1 — Shared HTTP request builder

**Goal:** One place that builds authenticated, headered, retry-wrapped
POST requests for both providers.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/request.rs` | New — `struct ProviderRequestBuilder` with `AuthStyle` enum and `build` + `send` methods |
| `crates/anie-providers-builtin/src/http.rs` | Update — share one `reqwest::Client` via `OnceCell` or similar; stop `.expect`ing in client construction |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Use `ProviderRequestBuilder`; delete duplicated header / status-check code |
| `crates/anie-providers-builtin/src/anthropic.rs` | Same; use `ProviderRequestBuilder` |

### Sub-step A — `AuthStyle` enum

```rust
pub enum AuthStyle {
    Bearer(String),          // OpenAI, OpenRouter, most OpenAI-compat
    Header {                 // Anthropic (x-api-key), others
        name: &'static str,
        value: String,
    },
    None,                    // local servers with no auth
}
```

### Sub-step B — `ProviderRequestBuilder`

```rust
pub struct ProviderRequestBuilder {
    client: reqwest::Client,
    url: String,
    auth: AuthStyle,
    custom_headers: Vec<(String, String)>,
    body: serde_json::Value,
}

impl ProviderRequestBuilder {
    pub fn new(url: impl Into<String>) -> Self;
    pub fn auth(mut self, auth: AuthStyle) -> Self;
    pub fn custom_headers(mut self, headers: &[(String, String)]) -> Self;
    pub fn body(mut self, body: serde_json::Value) -> Self;

    /// Send the request, returning the raw response. Status is NOT
    /// classified; see `send_and_classify` for that.
    pub async fn send(self) -> Result<reqwest::Response, ProviderError>;

    /// Send, and turn non-2xx into `ProviderError` via
    /// `classify_http_error`.
    pub async fn send_and_classify(self) -> Result<reqwest::Response, ProviderError>;
}
```

### Sub-step C — Fix `http.rs` to stop panicking

`http.rs:10` currently does `.expect(...)` on `reqwest::Client`
builder. TLS-roots loading can fail; make the client builder return
`Result` or wrap in a `OnceCell<Result<Client, ProviderError>>` so
the first failure is recorded and surfaced on every subsequent
attempt.

### Sub-step D — Migrate OpenAI

In `openai/mod.rs`, replace the body of the "send one request"
helper with a `ProviderRequestBuilder` construction. Preserve the
retry-around-send loop (that's plan 01's territory; don't rip it
out). The only thing that changes is how a single request is built
and sent.

### Sub-step E — Migrate Anthropic

Same idea. `anthropic.rs` currently sets `anthropic-version` as an
additional header; keep that. `AuthStyle::Header { name: "x-api-key",
value: key }` covers the auth.

### Test plan

| # | Test |
|---|------|
| 1 | `bearer_auth_adds_authorization_header` (build-only; no network) |
| 2 | `header_auth_adds_named_header` |
| 3 | `no_auth_adds_no_auth_header` |
| 4 | `custom_headers_are_applied_in_order` |
| 5 | `send_and_classify_maps_401_to_provider_error_auth` (`wiremock` or a hand-rolled `tokio::net::TcpListener` stub) |
| 6 | `send_and_classify_maps_429_to_provider_error_rate_limited` |
| 7 | `send_and_classify_maps_5xx_to_provider_error_http` |
| 8 | Existing integration tests pass unchanged. |

### Files that must NOT change

- `crates/anie-providers-builtin/src/sse.rs` — SSE parsing stays.
- `crates/anie-providers-builtin/src/local.rs` — heuristics only.
- Consumers in `anie-cli` / `anie-agent` — the `Provider` trait is
  unchanged.

### Exit criteria

- [ ] Both providers use `ProviderRequestBuilder`.
- [ ] `reqwest::Client` is constructed once per crate.
- [ ] No `.expect` in HTTP client construction.
- [ ] ~60 LOC of duplication deleted.

---

## Phase 2 — Unified tool-call reassembly

**Goal:** One `ToolCallAssembler` used by both providers' stream
state machines. Delete the divergent `started`/`ended` tracking in
OpenAI and the `None`-in-blocks-map tracking in Anthropic.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/streaming/tool_call_assembler.rs` | New — shared tool-call assembler |
| `crates/anie-providers-builtin/src/streaming/mod.rs` | New or existing — re-export |
| `crates/anie-providers-builtin/src/openai/streaming.rs` | Use shared assembler; delete `OpenAiToolCallState` |
| `crates/anie-providers-builtin/src/anthropic.rs` | Use shared assembler; delete inlined tool-use tracking |

### Sub-step A — Design the assembler

```rust
pub struct ToolCallAssembler {
    // index → partial state
    slots: BTreeMap<u32, PartialToolCall>,
}

struct PartialToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    started_emitted: bool,
    ended_emitted: bool,
}

pub enum ToolCallStep {
    Started { index: u32, id: String, name: String },
    ArgsDelta { index: u32, delta: String },
    Ended { index: u32, arguments_json: String },
}

impl ToolCallAssembler {
    pub fn ingest_id(&mut self, index: u32, id: String) -> Option<ToolCallStep>;
    pub fn ingest_name(&mut self, index: u32, name: String) -> Option<ToolCallStep>;
    pub fn ingest_args(&mut self, index: u32, delta: String) -> Option<ToolCallStep>;
    pub fn finish(&mut self) -> Vec<ToolCallStep>;
}
```

The provider-specific code translates its wire events (OpenAI's
`choice.delta.tool_calls[i].{id, function.name,
function.arguments}`; Anthropic's `content_block_start`,
`content_block_delta`) into calls to these four methods, and emits
the returned `ToolCallStep`s as `ProviderEvent`s.

### Sub-step B — Migrate OpenAI

Delete `OpenAiToolCallState`. Route each `tool_calls` delta into the
assembler.

### Sub-step C — Migrate Anthropic

Delete the inlined tool-use tracking. Route `content_block_start` /
`content_block_delta` for `tool_use` blocks into the assembler.

### Test plan

| # | Test |
|---|------|
| 1 | `id_then_name_emits_started` |
| 2 | `name_then_id_also_emits_started` (order-independence) |
| 3 | `multiple_args_deltas_accumulate` |
| 4 | `finish_emits_ended_for_each_slot` |
| 5 | `finish_produces_parseable_arguments_json` |
| 6 | `two_indices_track_independently` |
| 7 | Existing provider integration tests pass. |

### Exit criteria

- [ ] One tool-call assembler, two consumers.
- [ ] `OpenAiToolCallState` deleted.
- [ ] Anthropic's inlined tracking deleted.
- [ ] Unit tests cover ordering and multi-index cases.

---

## Phase 3 — Unified model discovery

**Goal:** One `discover` function that dispatches on `ApiKind`.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/model_discovery.rs` | Collapse `discover_openai_compatible_models`, `discover_anthropic_models`, `discover_ollama_tags` into `async fn discover(api_kind, endpoint, auth) -> Result<Vec<Model>, ProviderError>` plus small per-kind parse helpers |
| `crates/anie-providers-builtin/src/model_discovery/parse.rs` | New *(optional)* — per-kind JSON shape parsing helpers, if the file would grow past 700 LOC otherwise |

### Sub-step A — Signature

```rust
pub async fn discover(
    api_kind: ApiKind,
    endpoint: &str,
    auth: AuthStyle,
) -> Result<Vec<Model>, ProviderError>;
```

Inside, a `match api_kind { ApiKind::OpenAICompletions => ...,
ApiKind::Anthropic => ..., ApiKind::OllamaTags => ... }` selects the
URL path and response-parsing helper.

Use `ProviderRequestBuilder` from phase 1 for the actual request.

### Sub-step B — Parse helpers

Extract the JSON → `Vec<Model>` mapping for each kind into a named
helper:

```rust
fn parse_openai_models(json: &serde_json::Value) -> Result<Vec<Model>, ProviderError>;
fn parse_anthropic_models(json: &serde_json::Value) -> Result<Vec<Model>, ProviderError>;
fn parse_ollama_tags(json: &serde_json::Value) -> Result<Vec<Model>, ProviderError>;
```

Each is a pure function, unit-testable without a network.

### Sub-step C — Migrate callers

Callers in `anie-cli` and the onboarding flow currently reference
the kind-specific functions. Update to `discover(api_kind, ...)`.

### Test plan

| # | Test |
|---|------|
| 1 | `parse_openai_models_extracts_id_and_name` (static JSON input) |
| 2 | `parse_openai_models_skips_non_chat_models` (if that filter exists today) |
| 3 | `parse_anthropic_models_reads_model_list` |
| 4 | `parse_ollama_tags_reads_tag_list` |
| 5 | `discover_dispatches_to_correct_parser_by_api_kind` (mocked HTTP) |
| 6 | Existing `model_discovery` tests pass unchanged. |

### Files that must NOT change

- `crates/anie-provider/src/model.rs` — `Model` struct stays as is.

### Exit criteria

- [ ] One `discover` entry point.
- [ ] `model_discovery.rs` is ≤ 700 LOC (from 925).
- [ ] Parse helpers are pure and unit-tested.
- [ ] No caller needs to know which internal function to call.

---

## Files that must NOT change in any phase

- `crates/anie-protocol/*` — wire format is untouched.
- `crates/anie-provider/src/provider.rs` — trait signature unchanged.
- `crates/anie-provider/src/registry.rs` — registry API unchanged.
- `crates/anie-tui/*` — overlays use the public discovery function
  via `anie-cli` indirection; no UI code changes.

## Dependency graph

```
Phase 1 (request builder)
  └─► Phase 2 (tool-call assembler)    ─┐
  └─► Phase 3 (unified discovery)      ─┤
                                        └──► (complete)
```

Phase 1 blocks both 2 and 3 because they use the request builder
and shared client. Phases 2 and 3 are independent of each other.

## Out of scope

- Error taxonomy tightening — that's plan 05.
- OAuth auth styles — tracked in `docs/ideas.md`. `AuthStyle` can
  add a variant later.
- Adding new provider kinds (Google, Mistral, etc.) — tracked in
  `docs/ideas.md`. This plan makes that cheaper, but does not
  include it.
- Caching discovery results to disk — separate feature.
