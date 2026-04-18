# Plan 04 — Provider unification (narrowed)

> **Revised 2026-04-17.** Originally proposed a workspace-wide
> `ProviderRequestBuilder`. After comparison with pi-mono
> (`pi_mono_comparison.md`), the full builder is **not landing** —
> pi deliberately keeps providers independent for readability, and
> anie should follow that shape. Plan 04 is narrowed to:
>
> 1. A tiny shared helper for HTTP client + status classification
>    (much smaller than the original `ProviderRequestBuilder`).
> 2. A shared `ToolCallAssembler` (unchanged).
> 3. Unified model discovery (unchanged).
>
> The per-request body construction stays in each provider file.

> **Status (2026-04-17):**
> - **Phase 1 (shared HTTP client):** `7948971`. Lazy-inited
>   `OnceLock<Result<Client, _>>` in `http.rs`; both providers
>   pull from it at `::new()` with a fallback to
>   `create_http_client()` on init failure (preserves existing
>   infallible `::new()` signature). `classify_http_error` in
>   `util.rs` was already shared — no change needed there.
> - **Phase 3 (unified discovery) — already factored.** Upon
>   inspection, `discover_models` in `model_discovery.rs` already
>   dispatches on `ApiKind` to per-API helpers
>   (`discover_openai_compatible_models`,
>   `discover_anthropic_models`, `discover_ollama_tags`). The
>   shared pieces (`send_request`, `discovery_http_client`,
>   `normalize_*_base_url`, `build_headers`) are already extracted.
>   Further collapsing the three per-API functions into one would
>   only merge the response-parsing code, which is genuinely
>   different per API and would hurt readability. No action.
> - **Phase 2 (ToolCallAssembler) — deferred.** On inspection,
>   OpenAI's model (`arguments: String` accumulated, JSON-parsed at
>   finish) and Anthropic's model (`partial_json` + pre-parsed
>   `input: serde_json::Value` from `input_json_delta` events) are
>   structurally different. A shared assembler would either pick
>   one shape and wrap the other (imposing overhead) or parameterize
>   heavily (defeating the point). Best left in place unless a
>   third provider creates a clearer generalization target.

## Motivation

`crates/anie-providers-builtin/src/anthropic.rs` (687 LOC) and
`crates/anie-providers-builtin/src/openai.rs` (2084 LOC) share some
shape but diverge meaningfully:

- Request body construction is **genuinely different** — Anthropic's
  `cache_control`, `thinking` blocks, `betas` header, tool-call
  format, and stop-reason mapping are not compatible with OpenAI's
  request shape. pi confirms this: each provider file in
  `~/Projects/agents/pi/packages/ai/src/providers/` is 500–1000
  LOC and does its own request building. pi only shares the thin
  pieces (event stream helpers, message-transform helpers,
  responses-API-specific base).

- What **does** repeat:
  - HTTP client construction (`reqwest::Client` should be one per
    crate, not one per call).
  - Status-code → `ProviderError` classification.
  - Tool-call streaming reassembly logic (both providers track
    id + name + arguments across chunks; the shapes are
    near-identical).
  - Model discovery (three parallel `discover_*` functions).

- What **should NOT** be unified:
  - Auth header construction (OpenAI `bearer_auth`, Anthropic
    `x-api-key`, custom providers may do something different). A
    trivial match on a handful of styles lives inside each
    provider and stays small. A "builder" abstracts nothing
    useful.
  - Request body. Nothing cross-provider about it.
  - Stream state machines (the reasoning handling is
    provider-specific enough that a shared state machine would
    fight its consumers; see plan 01 for how OpenAI's state
    machine gets split internally).

## Design principles

1. **Match pi's shape.** Providers are independent files. Shared
   code lives in narrow, purpose-specific helpers.
2. **Share the HTTP client, not the builder.** One
   `reqwest::Client` per crate, accessed via a module function.
3. **Share tool-call reassembly.** The one piece that's genuinely
   the same across providers.
4. **Unify discovery.** Three discovery functions with a shared
   suffix collapse into one dispatch.
5. **Keep auth inline.** Each provider's 5-line auth setup is not
   a refactoring opportunity — it's the clearest place to put it.

## Preconditions

Plan 01 (`openai.rs` module split) should land first. The
extractions here assume `openai/mod.rs` is the `Provider` impl
facade and that streaming/convert/reasoning_strategy are in their
own modules.

---

## Phase 1 — Shared HTTP client + error classification

**Goal:** One `reqwest::Client` per crate. One place for
status-code → `ProviderError`. Stop panicking on client
construction.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/http.rs` | Share one `reqwest::Client` via `OnceCell`; remove `.expect(...)` on client build; expose `client() -> Result<&'static Client, ProviderError>` |
| `crates/anie-providers-builtin/src/util.rs` | Keep `classify_http_error` here (or move if already elsewhere) as the single status → `ProviderError` mapper |
| `crates/anie-providers-builtin/src/openai/mod.rs` | Replace any per-call `Client::new()` / `Client::builder()` with `http::client()?` |
| `crates/anie-providers-builtin/src/anthropic.rs` | Same |

No new request-builder abstraction. Auth header construction stays
inline in each provider.

### Sub-step A — Shared client

```rust
// crates/anie-providers-builtin/src/http.rs

static CLIENT: OnceLock<Result<Client, ClientInitError>> = OnceLock::new();

pub fn client() -> Result<&'static Client, ProviderError> {
    match CLIENT.get_or_init(|| {
        Client::builder().build().map_err(|e| ClientInitError {
            message: e.to_string(),
        })
    }) {
        Ok(c) => Ok(c),
        Err(e) => Err(ProviderError::Transport(e.message.clone())),
    }
}
```

### Sub-step B — Single status classifier

`classify_http_error(status: StatusCode, body: &str) -> ProviderError`
lives in `util.rs`. Every non-2xx response goes through it. This
is the **only** place string-matching on error bodies happens
(e.g., "context_length_exceeded" → `ContextOverflow`).

### Sub-step C — Migrate OpenAI and Anthropic

Each provider:

1. At the top of the file, `use crate::http;`.
2. Where the provider currently builds a `Client`, call
   `http::client()?` instead.
3. Leave auth, custom headers, URL construction, and body
   construction exactly as they are.

### Test plan

| # | Test |
|---|------|
| 1 | `client_returns_ok_under_normal_tls_roots` |
| 2 | `client_returns_err_when_builder_fails` (harder to simulate; may document as "best-effort, verified by reading the code path") |
| 3 | `classify_http_error_maps_401_to_auth` |
| 4 | `classify_http_error_maps_429_to_rate_limited_with_retry_after` |
| 5 | `classify_http_error_maps_context_length_body_to_context_overflow` |
| 6 | `classify_http_error_maps_5xx_to_http` |
| 7 | Existing integration tests pass. |

### Files that must NOT change

- `crates/anie-providers-builtin/src/sse.rs` — SSE parsing stays.
- `crates/anie-providers-builtin/src/local.rs` — heuristics only.
- Consumers in `anie-cli` / `anie-agent` — the `Provider` trait is
  unchanged.
- Auth / header setup in each provider — stays inline.

### Exit criteria

- [ ] Single `reqwest::Client` per crate, accessed via
      `http::client()?`.
- [ ] No `.expect(...)` on `reqwest::Client::builder().build()`.
- [ ] `classify_http_error` is the only site that inspects raw
      response bodies.
- [ ] Each provider's auth/header block is smaller only where it
      stops building a client; everything else unchanged.

---

## Phase 2 — Shared `ToolCallAssembler`

**Goal:** One tool-call assembler used by both providers. Delete
divergent `started`/`ended` bookkeeping in OpenAI and the
`None`-in-blocks-map tracking in Anthropic.

### Files to change

| File | Change |
|------|--------|
| `crates/anie-providers-builtin/src/streaming/tool_call_assembler.rs` | New — shared tool-call assembler |
| `crates/anie-providers-builtin/src/streaming/mod.rs` | New — re-export |
| `crates/anie-providers-builtin/src/openai/streaming.rs` *(post-plan-01)* | Use shared assembler; delete `OpenAiToolCallState` |
| `crates/anie-providers-builtin/src/anthropic.rs` | Use shared assembler; delete inlined tool-use tracking |

### Sub-step A — Design the assembler

```rust
pub struct ToolCallAssembler {
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

Provider-specific code translates its wire events into calls on
the assembler, and emits returned `ToolCallStep`s as
`ProviderEvent`s.

### Sub-step B — Migrate OpenAI

Delete `OpenAiToolCallState`. Route each `tool_calls` delta into
the assembler.

### Sub-step C — Migrate Anthropic

Delete the inlined tool-use tracking. Route
`content_block_start` / `content_block_delta` for `tool_use`
blocks into the assembler.

### Test plan

| # | Test |
|---|------|
| 1 | `id_then_name_emits_started` |
| 2 | `name_then_id_also_emits_started` (order-independence) |
| 3 | `multiple_args_deltas_accumulate` |
| 4 | `finish_emits_ended_for_each_slot` |
| 5 | `finish_produces_parseable_arguments_json` |
| 6 | `two_indices_track_independently` |
| 7 | `args_before_id_or_name_buffers_correctly` |
| 8 | Existing provider integration tests pass. |

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
| `crates/anie-providers-builtin/src/model_discovery.rs` | Collapse `discover_openai_compatible_models`, `discover_anthropic_models`, `discover_ollama_tags` into `async fn discover(api_kind, endpoint, auth) -> Result<Vec<Model>, ProviderError>` plus pure per-kind parse helpers |
| *(optional)* `crates/anie-providers-builtin/src/model_discovery/parse.rs` | New — per-kind JSON shape parsing helpers, if the file would grow past 700 LOC otherwise |

### Sub-step A — Signature

```rust
pub enum DiscoveryAuth {
    Bearer(String),
    Header { name: &'static str, value: String },
    None,
}

pub async fn discover(
    api_kind: ApiKind,
    endpoint: &str,
    auth: DiscoveryAuth,
) -> Result<Vec<Model>, ProviderError>;
```

Note: `DiscoveryAuth` is **local to the discovery module** — not a
shared workspace auth abstraction. Each call site constructs it
inline. Kept small and discovery-specific.

Inside, a `match api_kind { ApiKind::OpenAICompletions => ...,
ApiKind::Anthropic => ..., ApiKind::OllamaTags => ... }` selects
the URL path and response-parsing helper.

Uses `http::client()?` from phase 1 for the actual request.

### Sub-step B — Pure parse helpers

Extract the JSON → `Vec<Model>` mapping for each kind into a named
helper:

```rust
fn parse_openai_models(json: &serde_json::Value) -> Result<Vec<Model>, ProviderError>;
fn parse_anthropic_models(json: &serde_json::Value) -> Result<Vec<Model>, ProviderError>;
fn parse_ollama_tags(json: &serde_json::Value) -> Result<Vec<Model>, ProviderError>;
```

Each is pure, unit-testable without a network.

### Sub-step C — Migrate callers

Callers in `anie-cli` and the onboarding flow reference the
kind-specific functions today. Update to `discover(api_kind,
...)`.

### Test plan

| # | Test |
|---|------|
| 1 | `parse_openai_models_extracts_id_and_name` (static JSON) |
| 2 | `parse_openai_models_skips_non_chat_models` (if that filter exists) |
| 3 | `parse_anthropic_models_reads_model_list` |
| 4 | `parse_ollama_tags_reads_tag_list` |
| 5 | `discover_dispatches_to_correct_parser_by_api_kind` (mocked HTTP) |
| 6 | Existing `model_discovery` tests pass unchanged. |

### Files that must NOT change

- `crates/anie-provider/src/model.rs` — `Model` struct stays as is.

### Exit criteria

- [ ] One `discover` entry point.
- [ ] `model_discovery.rs` is ≤ 700 LOC (down from 925).
- [ ] Parse helpers are pure and unit-tested.
- [ ] No caller needs to know which internal function to call.

---

## What was removed from the original plan

The original plan 04 included a phase 1 with:

```rust
pub struct ProviderRequestBuilder {
    client: reqwest::Client,
    url: String,
    auth: AuthStyle,
    custom_headers: Vec<(String, String)>,
    body: serde_json::Value,
}
```

and a shared `AuthStyle` enum spanning both providers.

**Why it was dropped:** pi's providers each build their own
requests. The duplication between anthropic.rs and
openai-completions.ts in pi is accepted because the request bodies
and header rituals are meaningfully different. A shared builder
covers only a thin outer shell (headers, auth) and leaves the
body construction — the actual bulk of each call — unchanged.
That's not enough payoff for the abstraction overhead.

If a third or fourth provider arrives and genuinely wants the same
auth + header + URL scaffolding, revisit. Until then, prefer
duplication over premature abstraction. This is explicitly pi's
stance.

---

## Files that must NOT change in any phase

- `crates/anie-protocol/*` — wire format is untouched.
- `crates/anie-provider/src/provider.rs` — trait signature
  unchanged.
- `crates/anie-provider/src/registry.rs` — registry API unchanged.
- `crates/anie-tui/*` — overlays use the public discovery function
  via `anie-cli` indirection; no UI code changes.
- Per-provider auth header construction — stays inline.

## Dependency graph

```
Phase 1 (shared client + classifier)
  └─► Phase 2 (tool-call assembler)   [independent of phase 3]
  └─► Phase 3 (unified discovery)     [independent of phase 2]
```

Phase 1 must land first (phases 2 and 3 both use `http::client()`).
Phases 2 and 3 are independent of each other.

## Out of scope

- A workspace-wide request builder abstraction — see note above.
- Error taxonomy tightening — that's plan 05.
- OAuth auth styles — tracked in `docs/ideas.md`.
- Adding new provider kinds (Google, Mistral, etc.) — tracked in
  `docs/ideas.md`.
- Caching discovery results to disk — separate feature.
