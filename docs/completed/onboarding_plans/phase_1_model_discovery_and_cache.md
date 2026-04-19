# Phase 1 — Model Discovery Service and Cache

This phase adds a provider-aware model discovery layer, a normalized `ModelInfo` type, and a TTL cache. It is backend-only — no UI changes land in this phase.

## Why this phase exists

Anie currently uses a **static** model catalog assembled at startup from three sources:

1. `builtin_models()` — hardcoded hosted provider models
2. `configured_models()` — custom entries from `config.toml`
3. `detect_local_servers()` — models discovered from local endpoints via `/v1/models`

That catalog is never refreshed at runtime. For a pi-style model picker, Anie must be able to ask a provider endpoint "what models do you offer right now?" at any point during a session — after pulling a new Ollama model, after adding a new API key, after switching providers.

### Architecture note: why not extend the `Provider` trait directly

`tmp.md` proposes adding `list_models()` to the `Provider` trait. That would be architecturally wrong for Anie today because:

- `ProviderRegistry` is keyed by `ApiKind` (`AnthropicMessages`, `OpenAICompletions`, etc.)
- one `OpenAIProvider` implementation serves dozens of named providers with different base URLs and auth
- model discovery is about a **specific endpoint instance** (base URL + auth + provider name), not about an API kind

The right first step is a **model discovery service** that accepts endpoint details as input and returns normalized results. The streaming `Provider` trait stays untouched.

---

## Current code facts

| Item | File | Notes |
|------|------|-------|
| `Model` struct | `crates/anie-provider/src/model.rs` | 14 fields including `api`, `base_url`, `provider` |
| `ApiKind` enum | `crates/anie-provider/src/api_kind.rs` | `AnthropicMessages`, `OpenAICompletions`, `OpenAIResponses`, `GoogleGenerativeAI` |
| `ProviderRegistry` | `crates/anie-provider/src/registry.rs` | keyed by `ApiKind`, stores `Box<dyn Provider>` |
| `probe_openai_compatible()` | `crates/anie-providers-builtin/src/local.rs` | already parses `/v1/models` → `Vec<Model>` |
| `detect_local_servers()` | `crates/anie-providers-builtin/src/local.rs` | probes `localhost:11434` and `localhost:1234` |
| `builtin_models()` | `crates/anie-providers-builtin/src/models.rs` | static list of 5 Anthropic + OpenAI models |
| `build_model_catalog()` | `crates/anie-cli/src/controller.rs` | merges built-in + configured + local at startup |
| HTTP client helper | `crates/anie-providers-builtin/src/http.rs` | `create_http_client()` with 300s timeout |

---

## Files expected to change

### New files

- `crates/anie-providers-builtin/src/model_discovery.rs` — discovery service and cache

### Modified files

- `crates/anie-providers-builtin/src/lib.rs` — re-export discovery API
- `crates/anie-provider/src/lib.rs` — re-export `ModelInfo` if placed in this crate
- `crates/anie-provider/src/model.rs` — add `ModelInfo` type (or a new `model_info.rs`)

### Not yet

- `crates/anie-tui/` — no UI changes
- `crates/anie-cli/src/controller.rs` — no wiring yet
- `crates/anie-config/` — no config changes

---

## Recommended implementation

### Sub-step A — Define `ModelInfo`

Place the display-oriented discovery type in `crates/anie-provider/` so both the TUI and CLI crates can reference it without depending on `anie-providers-builtin`.

```rust
/// A model discovered from a provider endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    /// Model identifier as reported by the endpoint.
    pub id: String,
    /// Human-readable display name (may equal `id`).
    pub name: String,
    /// Provider name (e.g. "openai", "ollama", "anthropic").
    pub provider: String,
    /// Context window size, if reported.
    pub context_length: Option<u64>,
    /// Whether the model accepts image input, if known.
    pub supports_images: Option<bool>,
    /// Whether the model supports reasoning features, if known.
    pub supports_reasoning: Option<bool>,
}
```

Key design choices:

- `Option` fields for metadata the endpoint may not expose
- no `base_url` or `api` — those belong to the discovery request context, not the result
- serializable for future cache persistence

### Sub-step B — Define `ModelDiscoveryRequest`

```rust
/// Parameters for a model discovery request against a specific provider endpoint.
#[derive(Debug, Clone)]
pub struct ModelDiscoveryRequest {
    /// The provider name for cache keying and display.
    pub provider_name: String,
    /// The API kind this endpoint speaks.
    pub api: ApiKind,
    /// The base URL of the endpoint.
    pub base_url: String,
    /// Optional API key for authenticated endpoints.
    pub api_key: Option<String>,
    /// Optional additional headers (e.g. `anthropic-version`).
    pub headers: HashMap<String, String>,
}
```

### Sub-step C — Implement the discovery service entry point

```rust
/// Discover models from a provider endpoint.
/// Returns the endpoint's model list or a structured error.
pub async fn discover_models(
    request: &ModelDiscoveryRequest,
) -> Result<Vec<ModelInfo>, ProviderError>;
```

Internally, dispatch on `request.api`:

| `ApiKind` | Endpoint | Auth | Notes |
|-----------|----------|------|-------|
| `OpenAICompletions` | `GET {base_url}/models` | Bearer token | Works for OpenAI, Groq, Together, Fireworks, xAI, local, and any OpenAI-compatible |
| `AnthropicMessages` | `GET {base_url}/v1/models` | `x-api-key` + `anthropic-version` header | Anthropic exposes this |
| `OpenAIResponses` | same as OpenAI | same | likely same endpoint |
| `GoogleGenerativeAI` | skip for now | — | can be added later |

For each backend:

1. build the HTTP request with auth
2. send with a **short timeout** (5–10s connect, 10s total)
3. parse the response JSON
4. normalize each model entry into `ModelInfo`

### Sub-step D — Refine Ollama-specific discovery

Ollama's `/api/tags` endpoint can expose richer local metadata than `/v1/models` (e.g. model family, parameter count, quantization).

Implement a secondary code path:

- if `provider_name == "ollama"` and `base_url` contains `:11434`
- try `GET {base_url}/api/tags` first
- fall back to `/v1/models` if `/api/tags` fails

Normalize into the same `ModelInfo` shape.

### Sub-step E — Add TTL cache

Create a cache type:

```rust
pub struct ModelDiscoveryCache {
    entries: HashMap<CacheKey, CacheEntry>,
    default_ttl: Duration,
}

struct CacheKey {
    provider_name: String,
    api: ApiKind,
    base_url: String,
    /// Hash of auth material — never store raw keys
    auth_fingerprint: u64,
}

struct CacheEntry {
    models: Vec<ModelInfo>,
    fetched_at: Instant,
}
```

Public API:

```rust
impl ModelDiscoveryCache {
    pub fn new(default_ttl: Duration) -> Self;

    /// Get models from cache if fresh, otherwise discover and cache.
    pub async fn get_or_discover(
        &mut self,
        request: &ModelDiscoveryRequest,
    ) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Force a refresh, bypassing the cache.
    pub async fn refresh(
        &mut self,
        request: &ModelDiscoveryRequest,
    ) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Invalidate all cached entries.
    pub fn clear(&mut self);
}
```

Cache rules:

- default TTL: 5 minutes
- explicit refresh bypasses TTL
- failed lookups are **not** cached (so retries work immediately)
- the cache is in-memory only (no disk persistence in v1)

### Sub-step F — Add `ModelInfo` → `Model` conversion helper

The rest of Anie works with `anie_provider::Model` (14 fields). Discovered `ModelInfo` has fewer fields. Add a conversion helper that fills in conservative defaults:

```rust
impl ModelInfo {
    pub fn to_model(&self, api: ApiKind, base_url: &str) -> Model {
        Model {
            id: self.id.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            api,
            base_url: base_url.to_string(),
            context_window: self.context_length.unwrap_or(32_768),
            max_tokens: 8_192,
            supports_reasoning: self.supports_reasoning.unwrap_or(false),
            reasoning_capabilities: None,
            supports_images: self.supports_images.unwrap_or(false),
            cost_per_million: CostPerMillion::zero(),
        }
    }
}
```

### Sub-step G — Re-export from `anie-providers-builtin`

In `crates/anie-providers-builtin/src/lib.rs`:

```rust
pub use model_discovery::{
    ModelDiscoveryCache, ModelDiscoveryRequest, discover_models,
};
```

And re-export `ModelInfo` from `crates/anie-provider/src/lib.rs`.

---

## Constraints

1. **Do not modify the `Provider` trait.** Discovery is a separate service.
2. **Do not add UI code.** Phase 2 handles that.
3. **Do not block the TUI render loop.** Discovery is async and called from background tasks.
4. **Do not log raw API keys.** Cache keys use a hash fingerprint.
5. **Degrade gracefully.** If an endpoint does not expose metadata (context length, capabilities), use `None` / conservative defaults.

---

## Test plan

### Required unit tests

| # | Test | Location |
|---|------|----------|
| 1 | OpenAI-compatible discovery parses `/v1/models` JSON correctly | `anie-providers-builtin` |
| 2 | Anthropic discovery parses `/v1/models` response correctly | `anie-providers-builtin` |
| 3 | Ollama `/api/tags` parsing produces correct `ModelInfo` list | `anie-providers-builtin` |
| 4 | Auth headers are attached when `api_key` is present | `anie-providers-builtin` |
| 5 | Auth headers are omitted when `api_key` is `None` | `anie-providers-builtin` |
| 6 | Cache hit avoids duplicate network call | `anie-providers-builtin` |
| 7 | Cache miss triggers discovery | `anie-providers-builtin` |
| 8 | Explicit refresh bypasses cache | `anie-providers-builtin` |
| 9 | Discovery failure returns `ProviderError`, is not cached | `anie-providers-builtin` |
| 10 | Unknown/extra JSON fields do not break parsing | `anie-providers-builtin` |
| 11 | `ModelInfo::to_model()` fills conservative defaults | `anie-provider` |

### Integration tests (mock server)

| # | Test |
|---|------|
| 1 | Probe a mock OpenAI-compatible server, get model list, verify cache |
| 2 | Probe a mock Ollama `/api/tags` server, get model list |
| 3 | Discovery against unreachable endpoint returns error within timeout |

### Manual validation

1. Run against a real Ollama instance → verify model list matches `ollama list`
2. Run against OpenAI with a real API key → verify known models appear
3. Pull a new Ollama model, call refresh → verify new model appears
4. Remove API key → verify discovery returns auth error, not a crash

---

## Exit criteria

- [ ] `ModelInfo` exists as a shared type in `anie-provider`
- [ ] `discover_models()` works for OpenAI-compatible endpoints
- [ ] `discover_models()` works for Anthropic
- [ ] `discover_models()` works for Ollama (with `/api/tags` refinement)
- [ ] `ModelDiscoveryCache` provides TTL caching with explicit refresh
- [ ] `ModelInfo::to_model()` conversion helper exists
- [ ] all unit tests pass
- [ ] no TUI behavior has changed

---

## Follow-on phase

→ `phase_2_pi_style_selector_host_and_model_picker.md`
