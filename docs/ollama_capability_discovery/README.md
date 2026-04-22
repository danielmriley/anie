# Ollama capability discovery

**Replace substring family-name matching with authoritative
per-model capability probing on Ollama. Use the same probe to
discover per-model context length. Keep the design extensible
so other providers' capability sources can plug in without a
second rewrite.**

## The bug we hit

User reports two symptoms with `qwen3.5:9b` on local Ollama:

1. `HTTP 400 {"error":{"message":"think value \"low\" is not
   supported for this model","type":"api_error"}}` when a thinking
   level is set.
2. Thinking blocks still render when the user toggles thinking
   to `Off`.

Live probing confirmed the chain:

- **Symptom 1.** `crates/anie-providers-builtin/src/local.rs:52-57`
  decides reasoning-capability via substring match:

  ```rust
  fn is_reasoning_capable_family(model_id: &str) -> bool {
      let model_id = model_id.to_ascii_lowercase();
      ["qwen3", "qwq", "deepseek-r1", "gpt-oss"]
          .iter()
          .any(|family| model_id.contains(family))
  }
  ```

  `"qwen3.5:9b".contains("qwen3")` is `true`. We mark
  `qwen3.5` as reasoning-capable with `ReasoningEffort` request
  mode, so we send `reasoning_effort: "low"` to Ollama's
  OpenAI-compat endpoint. Ollama translates that into its native
  `think: "low"` parameter, which qwen3.5's tokenizer/template
  doesn't support — only boolean `think: true|false`. 400.

  Confirmed via `curl http://localhost:11434/v1/chat/completions
  ... -d '{"reasoning_effort":"low"}'` reproducing the exact
  user-facing error.

  The same substring approach lives at
  `crates/anie-providers-builtin/src/model_discovery.rs:726-732`
  in `reasoning_family()` and at lines 712-723 in the fallback
  branch of `infer_reasoning()`. Same false positive there.

- **Symptom 2.** With no `reasoning_effort` field, Ollama
  *defaults to thinking-on* for any model whose `/api/show
  capabilities` includes `"thinking"` — and qwen3.5's does. The
  response carries a `reasoning` field that our streaming layer
  (`crates/anie-providers-builtin/src/openai/streaming.rs:119-125`)
  unconditionally extracts as a thinking delta. Off as
  field-omission isn't a real disable on Ollama. **This part is
  out of scope for this plan** — fixing it requires a
  native-`/api/chat` codepath with `think: false` (see
  Deferred). We address Symptom 1 plus the false-positive
  capability inference here.

## What pi-mono and codex do

Before designing further, we surveyed two reference implementations
that have shipped Ollama support: pi-mono
(`badlogic/pi-mono`, the TypeScript reference anie tracks) and
OpenAI's codex-rs (`openai/codex`).

### pi-mono's approach: explicit user config, no auto-discovery

pi-mono does **not** probe `/api/show` and does not infer
capabilities from model IDs. Instead, the user hand-curates
`~/.pi/agent/models.json` and declares each model's capabilities:

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        { "id": "gpt-oss:20b", "reasoning": true },
        { "id": "llama3.1:8b", "reasoning": false }
      ]
    }
  }
}
```

The `Model` type carries `reasoning: boolean` as a static field.
`compat.thinkingFormat` selects the wire format from a typed
enum: `"reasoning_effort"`, `"zai"` (top-level
`enable_thinking`), `"qwen"` (top-level `enable_thinking`), or
`"qwen-chat-template"` (`chat_template_kwargs.enable_thinking`).
`compat.supportsReasoningEffort: false` lets users disable the
field entirely on servers that 400 on it.

The library "only includes models that support tool calling"
in its built-in catalog. Anything else is user-configured.

**What we adopt:** the `thinkingFormat`-style enum for
non-Ollama OpenAI-compat servers (vLLM, SGLang) running Qwen.
A user-overridable `models.json`-equivalent is deferred but
sketched in the Deferred section.

**What we don't adopt:** "make the user curate everything."
anie targets out-of-box discovery and Ollama is the most common
local-model entry point — making users hand-write JSON for
every model they pull is a regression vs. our current behavior.

### codex's approach: server-version gating + curated default

codex has a dedicated `codex-rs/ollama/` crate, distinct from
its generic OpenAI-compat layer. The crate uses the **native**
Ollama API (`/api/tags`, `/api/version`, `/api/pull`) and
treats `--oss` as a first-class CLI mode with `gpt-oss:20b` as
the default model
(`codex-rs/ollama/src/lib.rs:17`).

It also does **not** probe `/api/show`. What it does instead:

```rust
fn min_responses_version() -> Version { Version::new(0, 13, 4) }

pub async fn ensure_responses_supported(
    provider: &ModelProviderInfo,
) -> std::io::Result<()> {
    // Hits /api/version. If older than 0.13.4, reject with a
    // clear "upgrade Ollama" message.
}
```

Server-level capability gating: rather than ask "does this
*model* support thinking?", they ask "does this *server* support
the Responses API?" — and refuse to run if not.

For the actual Responses-API world they've moved to, capability
discovery is the model catalog itself, which (like pi) is
hand-curated.

**What we adopt:**
- The codex pattern of a **separate native-API codepath for
  Ollama-specific operations**, distinct from the generic
  OpenAI-compat path. Defer to a follow-up plan (referenced in
  Deferred), but earmark the architecture now.
- The pattern of probing **server version** at provider-init
  time as a precondition for capability features — `/api/show`
  has been stable since Ollama 0.1.x but if we ever want to
  guard newer endpoints (e.g. `/api/embeddings/show`), this is
  the shape.

**What we don't adopt:** the "curated default model" approach.
codex narrowly targets `gpt-oss`; anie aims to support whatever
the user has pulled. We need real capability data to deliver
that.

### Why we still want /api/show

Both pi and codex avoid `/api/show` because their UX assumptions
let them: pi's user explicitly declares capabilities; codex's
user explicitly opts into a curated `gpt-oss` default. anie's
target experience is "user runs `ollama pull qwen3.5:9b`, picks
it from the model picker, it just works." That requires auto-
discovery, and `/api/show` is the only authoritative source.
The N+1 fan-out cost is the price.

## The authoritative source

Ollama exposes capabilities per model at `POST /api/show`:

```bash
$ curl -s -X POST http://localhost:11434/api/show \
    -d '{"name":"qwen3.5:9b"}' | jq .capabilities
["completion", "vision", "tools", "thinking"]

$ curl -s -X POST http://localhost:11434/api/show \
    -d '{"name":"gemma3:1b"}' | jq .capabilities
["completion"]
```

This is the same field Ollama's own UI uses. It updates
automatically when new models or model families are added —
no anie-side maintenance required.

`/api/tags` (the cheap "list models" endpoint we currently
use) does **not** include `capabilities`. Verified empirically
on the user's local Ollama: every entry returned by `/api/tags`
has `capabilities: MISSING`. Per-model `/api/show` calls are
the only way to get this data without running an actual
inference request.

The same `/api/show` response also carries the model's
**architectural context length** in `model_info`, keyed by the
architecture name from `model_info["general.architecture"]`:

```bash
$ curl -s -X POST http://localhost:11434/api/show \
    -d '{"name":"qwen3.5:9b"}' | jq '.model_info | with_entries(
      select(.key | endswith(".context_length")))'
{ "qwen35.context_length": 262144 }
```

Since this plan already fans out `/api/show` per model, picking
up `context_length` is a free addition to the same probe.

`/api/tags` carries `details.context_length` and
`details.context_window` in the schema, but they're absent in
practice on the user's local Ollama (`MISSING` for every
entry). `/api/show` is the only reliable source.

## Design

### Data shape

Add a single field to `ModelInfo` at
`crates/anie-provider/src/model.rs:224`:

```rust
/// Provider-reported capability tokens (e.g. `"vision"`,
/// `"tools"`, `"thinking"`). Populated by Ollama's
/// `/api/show.capabilities` array. `None` for endpoints
/// that don't expose this. Distinct from
/// `supported_parameters`, which is OpenRouter's
/// request-side parameter list.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub provider_capabilities: Option<Vec<String>>,
```

Why a new field rather than reusing `supported_parameters`:

- `supported_parameters` is request-side ("can the request body
  carry `tools`, `reasoning_effort`, `tool_choice`?"). Ollama's
  `capabilities` is model-side ("can this *model* think, see
  images, call tools?"). Conflating them would lose information
  and confuse anyone reading the catalog later.
- Keeping them separate lets future providers (Anthropic's
  `capabilities` block, Gemini's `supportedGenerationMethods`,
  Bedrock's modelDetail capabilities) populate
  `provider_capabilities` without rewriting the OpenRouter
  filter logic. See "Forward-looking" below.

The field stays `Option<Vec<String>>` deliberately — opaque
strings, no enum. Different providers will report different
vocabularies and we don't want to gatekeep new tokens with a
schema bump every time. Translation into typed booleans
(`supports_reasoning`, `supports_images`) happens at the
discovery site.

### Capability translation (Ollama)

Inside `discover_ollama_tags`
(`model_discovery.rs:510-568`), after fetching `/api/tags`,
fan out a `/api/show` call per model and use the resulting
capability vector to populate the typed booleans:

| Capability token | Sets                             |
|------------------|----------------------------------|
| `"thinking"`     | `supports_reasoning = Some(true)`|
| `"vision"`       | `supports_images = Some(true)`   |
| `"tools"`        | (recorded in `provider_capabilities`; no typed boolean today — anie assumes tools are usable on every model) |
| anything else    | (recorded in `provider_capabilities` only) |

If the `/api/show` call fails for a specific model (HTTP error,
timeout, malformed JSON), fall back to today's
`reasoning_family` heuristic *just for that model* and log a
warning. This keeps discovery resilient on flaky local servers
without giving up the heuristic safety net.

### Request-time invariant: silently drop thinking on non-thinking models

The user's thinking level (`Off | Minimal | Low | Medium | High`)
is a **preference applied across model switches**, not a per-model
setting. When the active model can't think, the level is silently
ignored at request-build time — no prompt, no warning, no error,
no reset of the user's preference. Switching back to a
thinking-capable model re-applies the preference automatically.

This invariant is already implemented in the existing request
builder, but is implicit — emergent from two checks rather than
stated as a property:

1. `native_reasoning_request_strategies`
   (`crates/anie-providers-builtin/src/openai/mod.rs:307-367`)
   returns `NoNativeFields` whenever `effective_reasoning_capabilities`
   yields `None` or non-`Native` control.
2. The `NoNativeFields` branch in
   `build_request_body_with_native_reasoning_strategy`
   (`mod.rs:201-209`) gates on
   `model.supports_reasoning && !is_local_openai_compatible_target(model)`
   — so a non-thinking model (`supports_reasoning = false`)
   never gets `reasoning_effort` written, and a local model
   never gets it via this branch (the `NestedReasoning` /
   `TopLevelReasoningEffort` strategies handle locals only when
   capabilities declare them).

After PR 3+5, `supports_reasoning` is **dynamically populated
from `/api/show.capabilities`** rather than guessed from the model
ID. So the existing silent-drop logic kicks in correctly for
qwen3.5:9b, gemma3:1b, and any other non-thinking model the
user has pulled — without the user ever knowing they "shouldn't"
have set a thinking level.

We make this invariant explicit in tests (see PR 5 test list)
so a future refactor of the request-strategy resolver can't
silently regress to "throw an error" or "send the field anyway."

### Context length: discover here, honor in the native codepath

`/api/show` carries the model's architectural max context length
(see "The authoritative source" above for the wire shape). Today
anie defaults every Ollama model to 32 768 tokens
(`model.rs:347`), which is wrong both directions: too small for
qwen3.5:9b (256 k), too large for any model on a default Ollama
install (4 k served).

Two fields, two consumers, two places this matters:

| Consumer                       | Reads                       | Source today              | After this plan                  |
|--------------------------------|-----------------------------|---------------------------|----------------------------------|
| TUI status bar / model picker  | `Model.context_window`      | 32 k fallback             | discovered from `/api/show`      |
| Compaction `reserve_tokens`    | `Model.context_window`      | 32 k fallback             | discovered from `/api/show`      |
| Ollama runtime `num_ctx`       | (must be sent on the wire)  | not sent → defaults to 4 k| sent via native `/api/chat`      |

**The wire-side change requires the deferred native-`/api/chat`
codepath.** Verified empirically:

```bash
# OpenAI-compat: silently ignored
$ curl /v1/chat/completions ... -d '{"options":{"num_ctx":16384}, ...}'
$ curl /api/ps  →  context_length=4096

# Native /api/chat: actually applied
$ curl /api/chat ... -d '{"options":{"num_ctx":16384}, ...}'
$ curl /api/ps  →  context_length=16384
```

So context-length discovery and `num_ctx` passthrough must ship
together. **Shipping discovery alone is a regression**: the
agent would advertise 256 k to compaction, the conversation
would grow to ~250 k tokens before compaction fires, the request
would go via OpenAI-compat at Ollama's default 4 k, and Ollama
would silently truncate the prompt — the agent suddenly forgets
everything. This is *worse* than today, where the 32 k fallback
keeps compaction tighter than Ollama's truncation point.

Plan-level consequence: **PR 3 (`/api/show` capability probe)
extracts `context_length` into `ModelInfo.context_length` and
stores it, but `to_model` does NOT propagate the discovered
value to `Model.context_window` for Ollama until the deferred
native codepath ships.** Until then, Ollama models continue to
use the 32 k fallback for `Model.context_window`. The raw
discovered value rides along in `provider_capabilities` /
`ModelInfo.context_length` so the native plan picks it up
without re-discovering.

**`num_ctx` is set once per model load, not per request.**
Changing it forces Ollama to reload the model into VRAM. The
native codepath sets `num_ctx = Model.context_window` on the
first request after a model is selected, and that value stays
constant for the rest of the session. Switching to a different
model (different `num_ctx`) is when a reload is acceptable.
Future enhancement: a `/context-length` slash command lets the
user override the discovered value (deferred — see Deferred).

### Runtime translation: `Model.reasoning_capabilities`

`ModelInfo::to_model` (`model.rs:346-364`) currently sets
`reasoning_capabilities: None` and lets
`effective_reasoning_capabilities` fall back to
`default_local_reasoning_capabilities` (the substring heuristic).

After this plan: when the `ModelInfo` carries
`provider_capabilities` containing `"thinking"`, `to_model`
populates `reasoning_capabilities` directly with the
appropriate `ReasoningCapabilities` (Native control,
ReasoningEffort or NestedReasoning depending on backend) —
skipping the heuristic entirely. When the Ollama capabilities
exist but don't include `"thinking"`, set
`reasoning_capabilities = None` and `supports_reasoning = false`.
The heuristic only fires when capabilities are absent (non-Ollama
local servers, or Ollama with a probe failure).

This means **the substring-matching `is_reasoning_capable_family`
function stays for now** — it's the explicit fallback for servers
that don't publish capability data (LM Studio, vLLM, custom
local servers). We tighten it to exact prefix matches in the
same PR so Qwen3.5 stops being mis-classified even on the
fallback path.

### The retry safety net

Even with capability discovery, the runtime needs to handle
the case where:

- Discovery is stale (model changed support) or skipped (user
  pointed anie at a non-Ollama local server that lies about
  capabilities).
- A model declares `"thinking"` but only accepts boolean `think`,
  not leveled (the qwen3.5 case — the capability is honest, the
  level rejection is the surprise).

`looks_like_native_reasoning_compat_body` at
`crates/anie-providers-builtin/src/openai/reasoning_strategy.rs:201-220`
already retries with `NoNativeFields` on classified errors. Today
it matches `reasoning_effort` / `reasoning` strings in the body.
Add the Ollama-specific wordings:

- `think value "..." is not supported for this model` — leveled
  rejection on a thinking model.
- `"<model>" does not support thinking` — non-thinking model
  hit with `reasoning_effort` (defense-in-depth: shouldn't
  happen after capability discovery, but guards the case where
  it does).

With both wordings recognized, the typed
`NativeReasoningUnsupported` error fires and the existing retry
loop in `send_stream_request` (`mod.rs:275-300`) silently
re-issues without `reasoning_effort`. The user sees a successful
response instead of a 400.

### Discovery cost & caching

`/api/tags` returns N models. Naive: N+1 round-trips per
discovery (1 tags + N show). On a typical local Ollama with
5–20 models, this is 5–20 extra requests *only* on cache miss.
Mitigations:

- **Parallel fan-out.** `tokio::join_all` the `/api/show` calls.
  Each is a localhost JSON request — wall-clock cost is bounded
  by the slowest, not the sum.
- **Existing `ModelDiscoveryCache` TTL.** `model_discovery.rs:55-91`
  already caches the full discovery result for the cache TTL.
  Capability data rides along; we don't add a separate cache.
- **`/api/show` failure tolerance.** If the show endpoint is
  unreachable for a given model, we don't hold up discovery —
  fall back per-model and continue. The whole discovery only
  fails on a `/api/tags` failure, same as today.

Estimated wall-clock budget: <500ms for 20 models on local
Ollama (per-call latency is sub-25ms in our measurements). On
remote Ollama setups (rare but supported) it could grow; the
TTL cache absorbs the cost after the first hit per session.

### Adopted from pi-mono: extend `ThinkingRequestMode`

pi's `compat.thinkingFormat` enum has four variants where ours
has three. The two we don't have are real and address servers
we'll meet soon:

| pi variant            | What it does on the wire                                      | Where it's used           |
|-----------------------|---------------------------------------------------------------|---------------------------|
| `reasoning_effort`    | top-level `reasoning_effort: "low"\|...`                      | OpenAI, OpenRouter (top-level), most OpenAI-compat |
| `zai`                 | top-level `enable_thinking: bool`                             | Z.ai's GLM models         |
| `qwen`                | top-level `enable_thinking: bool`                             | vLLM / SGLang serving Qwen3+ |
| `qwen-chat-template`  | nested `chat_template_kwargs.enable_thinking: bool`           | vLLM with chat-template kwargs |

We add two variants to `ThinkingRequestMode`:

```rust
pub enum ThinkingRequestMode {
    PromptSteering,
    ReasoningEffort,
    NestedReasoning,
    /// Top-level `enable_thinking: bool`. Used by vLLM /
    /// SGLang serving Qwen3+ and by Z.ai GLM models. NOT
    /// honored by Ollama's OpenAI-compat layer (verified
    /// empirically: Ollama silently ignores the field).
    EnableThinkingFlag,
    /// Nested `chat_template_kwargs.enable_thinking: bool`.
    /// Used by vLLM with chat-template kwargs forwarding.
    /// Same Ollama caveat.
    ChatTemplateEnableThinking,
}
```

When set, the request builder emits the boolean form
(`true` for any thinking level except `Off`; `false` for `Off`).
This is the **only** OpenAI-compat path that can honor an
explicit `Off` on a Qwen3+ model — and it works on
vLLM/SGLang. It does NOT solve Symptom 2 on Ollama, where the
field is silently ignored. (See "Confirmed empirically"
below.)

These variants don't get auto-selected by Ollama discovery —
Ollama's capabilities array doesn't distinguish format types.
They're available for users who configure a vLLM endpoint
explicitly, or for future per-server compat overrides.

### Confirmed empirically: Ollama silently ignores Qwen-style flags

We tested whether pi's `qwen` and `qwen-chat-template` formats
work on Ollama for Symptom 2 mitigation:

```bash
$ curl -s -X POST http://localhost:11434/v1/chat/completions \
    -d '{"model":"qwen3.5:9b","messages":[{"role":"user","content":"hi"}],
         "stream":false,"enable_thinking":false}' | jq '.choices[0].message'
{
  "content": "Hi there! ...",
  "reasoning": "Thinking Process: ..."   # ← still emits reasoning
}

$ curl -s -X POST http://localhost:11434/v1/chat/completions \
    -d '{"model":"qwen3.5:9b","messages":[{"role":"user","content":"hi"}],
         "stream":false,"chat_template_kwargs":{"enable_thinking":false}}' \
  | jq '.choices[0].message.reasoning != null'
true   # ← still emits reasoning

$ curl -s -X POST http://localhost:11434/api/chat \
    -d '{"model":"qwen3.5:9b","messages":[{"role":"user","content":"hi"}],
         "stream":false,"think":false}' | jq '.message.thinking'
null   # ← native /api/chat with think:false: actually disabled
```

So **the only path that disables thinking on a Qwen3+ Ollama
model is the native `/api/chat` codepath with `think: false`**.
That's a real architectural addition — it gets its own
follow-up plan (see Deferred) modeled on codex's separate
`ollama/` crate. Without that codepath, the user's "Off"
preference cannot be honored on Ollama Qwen3+ models, full
stop.

### Forward-looking: other providers

We're touching `ModelInfo` and `discover_*` once — design the
addition so the next provider's capability data lands without
schema work.

| Provider          | Capability source                                     | Plan |
|-------------------|-------------------------------------------------------|------|
| Ollama            | `/api/show.capabilities` (per-model fan-out)          | This plan |
| OpenAI            | None exposed via `/v1/models`. Catalog-driven today.  | No change. Heuristic fallback in `infer_reasoning` keeps working. |
| OpenRouter        | `supported_parameters` already populated.             | No change. The two fields are intentionally distinct. |
| Anthropic         | `capabilities.{vision,reasoning}` already populated.  | No change. Could later add `provider_capabilities` for parity but no current use. |
| LM Studio         | None exposed.                                         | Heuristic fallback. Tighten to exact-prefix match in this PR. |
| vLLM / SGLang     | None for capabilities; uses `enable_thinking` for Qwen. | New `ThinkingRequestMode::EnableThinkingFlag` variant lets compat config drive Qwen3+ properly. |
| Gemini, Bedrock   | Provider-specific endpoints exist; not implemented.   | When added, populate `provider_capabilities` from those endpoints. |

The recipe other providers follow:

1. Discovery code calls the provider's capability source.
2. Translates known tokens to typed booleans
   (`supports_reasoning`, `supports_images`).
3. Stores the raw vector in `provider_capabilities` for
   downstream consumers and debugging.
4. `to_model` reads the typed booleans (today's path) plus the
   capability vector if it needs richer information.

Critically, we do **not** need a `Provider::discover_capabilities`
trait method or per-provider trait extension. The per-provider
discovery functions in `model_discovery.rs` already are the
extension point — adding capability probing there is the same
shape of code that's already there for Anthropic vs OpenAI vs
OpenRouter divergence.

### What we explicitly do NOT do here

- **No native `/api/chat` codepath.** Symptom 2 (thinking-on by
  default) needs `think: false` via Ollama's native API. That's
  a meaningful new code path and deserves its own plan. See
  Deferred.
- **No leveled-vs-boolean distinction in `provider_capabilities`.**
  Ollama doesn't expose this and we've decided to keep levels.
  The runtime retry handles the mismatch when it occurs.
- **No removal of `is_reasoning_capable_family` /
  `reasoning_family`.** Both stay as the heuristic fallback for
  capability-less servers. We just stop them being the *primary*
  signal for Ollama.
- **No change to the heuristic substrings**, beyond tightening
  the family list to exact prefixes (e.g. `qwen3:`, `qwen3-`)
  so Qwen3.5 isn't caught on the fallback path.

## Files to touch

- `crates/anie-provider/src/model.rs`
  - Add `ModelInfo.provider_capabilities` field.
  - Add `ThinkingRequestMode::EnableThinkingFlag` and
    `ThinkingRequestMode::ChatTemplateEnableThinking` variants
    (pi-mono parity for vLLM/SGLang Qwen).
  - `to_model`: when `provider_capabilities` carries `"thinking"`
    and the model's API kind is `OpenAICompletions` *and*
    provider is `ollama`, populate `reasoning_capabilities`
    directly with `Native + ReasoningEffort` instead of leaving
    `None`.
  - `to_model`: **for Ollama models, do NOT yet propagate
    `ModelInfo.context_length` to `Model.context_window`** — keep
    the existing 32 k fallback until the deferred native
    `/api/chat` codepath ships. The discovered value rides along
    in `ModelInfo.context_length` for the native plan to
    consume. See "Context length: discover here, honor in the
    native codepath" in Design for the regression rationale.
    Non-Ollama discovered context lengths flow through to
    `Model.context_window` unchanged (today's behavior).
- `crates/anie-providers-builtin/src/model_discovery.rs`
  - Extend `discover_ollama_tags` to fan out `/api/show` calls
    after parsing `/api/tags`, populate `provider_capabilities`
    on each `ModelInfo`, translate to typed booleans, and
    extract `context_length` into `ModelInfo.context_length`
    from `model_info["{arch}.context_length"]` where `{arch}` is
    `model_info["general.architecture"]`.
  - Tighten `reasoning_family` to exact-prefix matching:
    `family == "qwen3"` (not `contains`), and the same pattern
    in `infer_reasoning` for the model-id fallback. Use a
    `family_id_prefix_matches` helper to avoid the
    "qwen3.5 contains qwen3" trap.
- `crates/anie-providers-builtin/src/local.rs`
  - `probe_openai_compatible`: when probing a server we identify
    as Ollama, also fetch `/api/show` per model and pass the
    capability vector through. Existing
    `default_local_reasoning_capabilities` becomes the fallback
    for non-Ollama local servers.
  - Tighten `is_reasoning_capable_family` the same way as
    `reasoning_family` above.
- `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`
  - `looks_like_native_reasoning_compat_body`: add Ollama
    wordings (`"think value"`, `"does not support thinking"`).
- `crates/anie-providers-builtin/src/openai/mod.rs`
  - `build_request_body_with_native_reasoning_strategy`: handle
    new `EnableThinkingFlag` and `ChatTemplateEnableThinking`
    strategies, emitting `enable_thinking: bool` (top-level or
    nested under `chat_template_kwargs`).
  - `native_reasoning_request_strategies`: route
    `EnableThinkingFlag` / `ChatTemplateEnableThinking` request
    modes to their matching strategies.

## Phased PRs

One small change per PR. Each PR self-contained, tests + clippy
green, mergeable independently.

### PR 1 — Tighten the substring heuristic

**Why first:** ships the actual user-visible fix (Qwen3.5 stops
being mis-classified) in a tiny diff. No new field, no new
endpoint, no new code paths. Just a correctness fix to the
existing heuristic. If the rest of the plan slips, the user is
already unblocked.

**Files:**
- `local.rs::is_reasoning_capable_family` — exact-prefix
  matching. `qwen3:`, `qwen3-`, `qwq:`, `qwq-`, `deepseek-r1:`,
  `deepseek-r1-`, `gpt-oss:`, `gpt-oss-`. Same for the bare-id
  forms (`qwen3`, `qwq`, etc. as a full prefix split on `:` /
  `-` / `.`).
- `model_discovery.rs::reasoning_family` and
  `infer_reasoning` — same change.
- New tests:
  - `qwen3_5_is_not_classified_as_reasoning_capable_family`
  - `qwen3_32b_remains_classified_as_reasoning_capable_family`
  - `gpt_oss_remains_classified` and equivalents for `qwq`,
    `deepseek-r1`.

### PR 2 — Add the retry safety net for Ollama wordings

**Why second:** independent of capability discovery, addresses
the *next* user who runs into a thinking-mismatch we didn't
anticipate. Lets PR 3+4 land without users seeing 400s in the
window between deploys.

**Files:**
- `openai/reasoning_strategy.rs::looks_like_native_reasoning_compat_body`
  — extend the body-pattern detection.
- New tests:
  - `classify_openai_http_error_recognizes_ollama_leveled_think_rejection`
    — body `think value "low" is not supported for this model`.
  - `classify_openai_http_error_recognizes_ollama_no_thinking_capability`
    — body `"gemma3:1b" does not support thinking`.
  - Negative: `classify_openai_http_error_does_not_misclassify_unrelated_400s`
    — body about a missing `messages` field stays
    `Http { status: 400 }`.

### PR 3 — Add `ModelInfo.provider_capabilities` and populate from Ollama

**Files:**
- `crates/anie-provider/src/model.rs` — add the field with
  `#[serde(default, skip_serializing_if = "Option::is_none")]`.
  No version bump needed (additive optional field per the
  CLAUDE.md convention).
- `model_discovery.rs::discover_ollama_tags` — fan out
  `/api/show` calls in parallel via `futures::future::join_all`,
  populate `provider_capabilities` on each `ModelInfo`.
  Translate `"thinking"` → `supports_reasoning = Some(true)`
  and `"vision"` → `supports_images = Some(true)`. Keep the
  existing heuristic-based values as the fallback when the show
  call fails for a model.
- New helper: `fetch_ollama_show_capabilities(client, base_url,
  model_id) -> Result<Option<Vec<String>>, ProviderError>`.
- New tests:
  - `ollama_discovery_uses_show_capabilities_when_available` —
    mock `/api/tags` plus per-model `/api/show` returning
    `{"capabilities":["completion","thinking"]}`. Assert
    `supports_reasoning = Some(true)` and
    `provider_capabilities` carries `["completion","thinking"]`.
  - `ollama_discovery_falls_back_to_heuristic_when_show_fails` —
    `/api/tags` succeeds, `/api/show` 500s for one entry. Assert
    discovery still returns all entries; the failed-show entry
    falls back to the heuristic.
  - `ollama_show_failure_does_not_fail_overall_discovery` —
    same as above, with assertion on the warning being logged.
  - `qwen3_5_via_show_capabilities_is_thinking_capable` — the
    real-world case: capability says yes, our heuristic would
    say yes too (substring match), but we get the *right*
    answer via authoritative data.
  - Forward-compat: `unknown_capability_token_is_preserved_in_provider_capabilities`.
  - `ollama_show_extracts_context_length_using_architecture_prefix`
    — `model_info["general.architecture"] = "qwen35"` plus
    `model_info["qwen35.context_length"] = 262144` produces
    `ModelInfo.context_length = Some(262144)`.
  - `ollama_show_handles_missing_context_length_field` —
    when no `{arch}.context_length` key exists, fall back to
    `ModelInfo.context_length = None` (no panic).
  - `to_model_does_not_propagate_ollama_context_length_until_native_path`
    — `ModelInfo` with `provider = "ollama"` and
    `context_length = Some(262144)` produces a `Model` with
    `context_window = 32_768` (the existing fallback). This is
    the explicit regression guard: discovery succeeds, display
    stays at the conservative value until the native codepath
    can honor it on the wire. Comment in the test names the
    deferred plan that flips this.
  - `to_model_propagates_non_ollama_context_length_unchanged` —
    `ModelInfo` with `provider = "openrouter"` and
    `context_length = Some(200_000)` produces a `Model` with
    `context_window = 200_000`. Non-Ollama paths are unaffected.

### PR 4 — Add `EnableThinkingFlag` / `ChatTemplateEnableThinking` request modes

**Why fourth, not first:** these don't fix the user's reported
Ollama bug (Ollama silently ignores both fields — verified). They
exist so that vLLM and SGLang users running Qwen3+ models behind
the OpenAI-compat shim can disable thinking with `Off` and
enable it on demand. Adopted from pi-mono's `compat.thinkingFormat`
enum (`qwen` and `qwen-chat-template` variants).

**Files:**
- `crates/anie-provider/src/model.rs` — add the two
  `ThinkingRequestMode` variants. Their `serde` rename matches
  pi: `enable_thinking_flag` and `chat_template_enable_thinking`.
- `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs`
  — extend `NativeReasoningRequestStrategy` with
  `EnableThinkingFlag { nested: bool }` (single variant, nested
  flag selects between top-level and `chat_template_kwargs`).
- `crates/anie-providers-builtin/src/openai/mod.rs` — handle
  the new strategy in
  `build_request_body_with_native_reasoning_strategy`. For
  `Off`, emit `false`. For any non-Off level, emit `true`.
- `crates/anie-providers-builtin/src/openai/mod.rs` — route
  the new request modes in
  `native_reasoning_request_strategies`.
- New tests:
  - `qwen_enable_thinking_flag_emits_top_level_boolean` —
    `Off → false`, `Low/Medium/High → true`.
  - `qwen_chat_template_enable_thinking_emits_nested_boolean` —
    same, under `chat_template_kwargs`.
  - `enable_thinking_flag_falls_back_to_no_native_fields_on_400`
    — same retry-with-fallback semantics as
    `ReasoningEffort`.
  - Forward-compat: existing `ThinkingRequestMode` variants
    behave unchanged.

### PR 5 — Wire `provider_capabilities` into `Model.reasoning_capabilities`

**Files:**
- `model.rs::to_model` — populate
  `reasoning_capabilities = Some(...)` when
  `provider == "ollama"` and `provider_capabilities` contains
  `"thinking"`. Otherwise keep `None` (existing fallback path).
- `local.rs::probe_openai_compatible` — when the probed server
  is Ollama, also fetch `/api/show` capabilities per model and
  attach. (Mirror of PR 3, for the local-server-detection path.)
- New tests:
  - `to_model_populates_reasoning_capabilities_from_ollama_thinking_capability`
    — `ModelInfo` with `provider_capabilities = ["thinking"]`
    produces a `Model` with `reasoning_capabilities = Native +
    ReasoningEffort`.
  - `to_model_leaves_reasoning_capabilities_none_when_thinking_absent`
    — same flow but capabilities = `["completion"]`.
  - `effective_reasoning_capabilities_prefers_declared_over_heuristic`
    — `Model` with `reasoning_capabilities = Some(...)` and a
    model id that the heuristic would mis-classify still
    returns the declared value, never the heuristic.
  - `local_probe_attaches_show_capabilities_for_ollama` — mirror
    of the discovery test, on the probe path.
  - **Invariant test:**
    `non_thinking_ollama_model_silently_drops_user_thinking_level`
    — `Model` discovered with capabilities = `["completion"]`,
    `StreamOptions.thinking = ThinkingLevel::Low`. Assert
    serialized request body has **no** `reasoning_effort`,
    `reasoning`, `enable_thinking`, or `chat_template_kwargs`
    field. Assert no error is produced. (This is the explicit
    silent-drop guarantee — covers Symptom 1 end-to-end.)
  - **Invariant test (hosted):**
    `non_thinking_hosted_model_silently_drops_user_thinking_level`
    — same but for a hosted (`is_local_openai_compatible_target =
    false`) model with `supports_reasoning = false`. Same
    assertion. Guards against a refactor that conditions only
    on local-vs-hosted instead of capability.
  - **Invariant test (level persistence):**
    `switching_to_non_thinking_model_preserves_user_thinking_preference`
    — set thinking to `Medium`, switch active model to a
    non-thinking one, switch back to a thinking-capable one,
    assert thinking is still `Medium`. Lives in
    `anie-cli/src/runtime/config_state.rs` tests next to the
    existing `apply_session_overrides_updates_current_model_and_thinking`
    test.

## Test plan

Per-PR tests above. Cross-cutting:

| # | Test | Where |
|---|------|-------|
| Manual | qwen3.5:9b on local Ollama — thinking on (any level), thinking off, with tools, without tools. None should produce a 400. The "off" case may still emit thinking until the deferred native-API plan; verify it doesn't *crash*. | smoke |
| Manual | qwen3:32b (genuine reasoning model) on local Ollama — thinking on (low/medium/high) all complete without 400. | smoke |
| Manual | gemma3:1b on local Ollama — thinking off completes without 400. Thinking on completes via the retry safety net (PR 2) without 400. | smoke |
| Manual | Switch from a thinking model (qwen3:32b) to a non-thinking model (gemma3:1b) without changing the thinking level. The non-thinking call must succeed silently — no error, no warning, no level reset. Switching back must restore thinking on the wire. | smoke |
| Manual | Discover qwen3.5:9b on local Ollama. `ModelInfo.context_length` must equal `262_144` (the architectural max from `/api/show`). The TUI status bar must continue showing `32 768` (the regression guard) until the deferred native plan ships. No truncation regressions in compaction. | smoke |
| Auto | `cargo test --workspace` green. | CI |
| Auto | `cargo clippy --workspace --all-targets -- -D warnings` clean. | CI |

## Risks

- **/api/show endpoint missing on older Ollama versions.** The
  endpoint has existed since Ollama 0.1.x and is documented in
  current docs. If a user runs a pre-historic Ollama, the
  show calls 404 and we fall back to the heuristic per-model.
  Mitigated by the per-model failure-tolerance design.

- **/api/show response shape changes.** Possible but unlikely.
  The `capabilities` array has been stable for the life of the
  endpoint. We use `#[serde(default)]` everywhere so a missing
  field doesn't break deserialization.

- **Discovery latency on slow networks.** Remote Ollama setups
  could see noticeable per-model latency × N. The TTL cache
  amortizes this. We can later add a "skip /api/show on remote
  Ollama" hint if it becomes a problem; default behavior still
  works (heuristic fallback).

- **Schema drift.** `ModelInfo.provider_capabilities` is an
  additive optional field. Older anie versions reading sessions
  written by newer versions just ignore it. Forward-compat test
  guards this.

- **Other local servers lie about being Ollama.** Some users
  point LM Studio at port 11434 or run a custom proxy. Our
  Ollama detection (`provider == "ollama" || base_url.contains
  (":11434")`) would mis-fire `/api/show` calls against them and
  get 404s. That's fine — the failure-tolerance path catches it
  and we fall back to the heuristic. No user-visible breakage.

## Exit criteria

- [ ] PR 1 merged: Qwen3.5:9b is no longer classified as
      reasoning-capable by the heuristic.
- [ ] PR 2 merged: a 400 from Ollama with a `think`-related
      message is silently retried without `reasoning_effort`.
- [ ] PR 3 merged: `/api/show capabilities` is the primary
      signal for Ollama discovery; `ModelInfo.provider_capabilities`
      populated. `ModelInfo.context_length` populated from
      `/api/show` but **not** propagated to
      `Model.context_window` for Ollama (regression guard
      pending native codepath).
- [ ] PR 4 merged: `EnableThinkingFlag` /
      `ChatTemplateEnableThinking` request modes available for
      vLLM/SGLang Qwen, with tests proving the wire shape.
- [ ] PR 5 merged: `Model.reasoning_capabilities` is populated
      from authoritative Ollama capabilities; heuristic only
      fires for capability-less servers.
- [ ] Manual smoke (table above) passes for qwen3.5:9b, qwen3:32b,
      gemma3:1b.
- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] No unflagged anie-specific deviation: every place we choose
      not to use `/api/show` is commented with the reason
      (per CLAUDE.md §3).

## Deferred

- **Native `/api/chat` codepath with boolean `think` and
  per-load `num_ctx`** — *modeled on codex's separate
  `codex-rs/ollama/` crate.* Two motivations, one codepath:
  (a) actually disable thinking on capable Ollama models when
  the user picks `Off`; (b) honor each model's discovered
  context length on the wire instead of getting Ollama's 4 k
  default. Empirically the OpenAI-compat layer silently ignores
  both `think` and `options.num_ctx` (verified with curl
  against `/api/ps`), so the native endpoint is the only path
  that works for either.

  Architecturally, a separate
  `crates/anie-providers-builtin/src/ollama/` module (mirroring
  codex's pattern) talks to `/api/chat` natively, with a switch
  in the runtime to choose between OpenAI-compat and native
  paths based on `provider_capabilities` and the user's
  thinking preference.

  **`num_ctx` is set once per model load, not per request.**
  Changing `num_ctx` between requests forces Ollama to reload
  the model into VRAM. The native codepath sends
  `options.num_ctx = Model.context_window` on the first
  request after a model is selected, and that value stays
  constant for the rest of that model's session. Switching to
  a different model (different `num_ctx`) is when a reload is
  acceptable.

  This plan's deferred follow-up exit criteria:
  - [ ] `Off` on a capable Ollama model produces a response
        with no `thinking` field (verified against `/api/chat`).
  - [ ] `to_model` for Ollama propagates the discovered
        `ModelInfo.context_length` to `Model.context_window`
        (flips the regression guard from PR 3 of this plan).
  - [ ] First request after model selection sends
        `options.num_ctx = Model.context_window`; subsequent
        requests in the same session reuse the same value.
        Switching models triggers a single reload at the new
        `num_ctx`.
  - [ ] Manual smoke: `/api/ps` shows the loaded
        `context_length` matches what the TUI status bar
        displays.

  Tracked as a follow-up plan (`docs/ollama_native_chat/` —
  to be written). Until that plan lands: `Off` on a capable
  Ollama model means thinking still streams; Ollama models
  display the conservative 32 k context_window even though
  the discovered value is larger.

- **User-overridable `~/.anie/models.json`** —
  *modeled on pi-mono's user config.* Lets users correct
  discovery mistakes ("this model thinks but isn't being
  detected" / "this model says it thinks but really doesn't"),
  add models that don't appear in `/api/tags`, and override
  per-model `compat` knobs. pi's merge semantics — provider
  defaults + per-model overrides + custom models upserted by
  id — are well thought through and worth adopting verbatim.
  Out of scope here; needs its own plan
  (`docs/user_models_config/`). Without it, users have no
  escape hatch when discovery is wrong. PR 1 (tightened
  heuristic) keeps the worst-case false positive limited
  in the meantime.

- **Server-version probing** — *modeled on codex's
  `ensure_responses_supported`.* When we adopt features that
  need a minimum Ollama version (none today), gate them
  against `/api/version` at provider-init time. The `/api/show`
  endpoint we're adopting now is stable since Ollama 0.1.x
  so no gating is needed for this plan. Track the pattern for
  future use.

- **User-overridable context length via `/context-length`
  slash command.** Once the native `/api/chat` codepath ships
  and `num_ctx` is wired per-load, a slash command lets the
  user override the discovered architectural max with a smaller
  value (e.g. for a memory-constrained machine that can't load
  qwen3.5:9b at 256 k) or a larger value (rare — would only
  apply if the user has tuned Ollama to serve beyond the model's
  declared max). The override updates `Model.context_window`
  for the active session and triggers a model reload at the
  new `num_ctx`. Persists to runtime state for the next
  session. Out of scope here; depends on the native codepath
  landing first.

- **Translating `"tools"` capability into a typed boolean.**
  anie currently assumes every model is tool-capable on
  OpenAI-compat. If we ever surface a "this model can't run
  tools" UI affordance, that's where this would land — for now
  the data is preserved in `provider_capabilities` for whoever
  wants to read it.

- **Capability discovery for LM Studio / vLLM.** Neither has a
  documented endpoint equivalent to `/api/show`. If one ships,
  the same per-provider branch in `discover_openai_compatible_models`
  is where it goes. No work required today.

- **Capability discovery for Gemini / Bedrock.** Both have
  capability-bearing endpoints. When we land those providers
  (see `docs/add_providers/03_google_gemini.md`,
  `docs/add_providers/06_amazon_bedrock.md`), they populate
  `provider_capabilities` from their respective sources.

## Reference

### pi-mono and codex

- pi-mono `models.json` schema (user config + compat enum):
  `badlogic/pi-mono` →
  `packages/coding-agent/docs/models.md`
- pi-mono `Model<TApi>` type:
  `packages/ai/src/types.ts`
- pi-mono `compat.thinkingFormat` dispatch:
  `packages/ai/src/providers/openai-completions.ts:399-415`
- codex's separate Ollama crate:
  `openai/codex` →
  `codex-rs/ollama/{lib.rs,client.rs,url.rs}`
- codex `ensure_responses_supported` (server-version gating):
  `codex-rs/ollama/src/lib.rs:56-76`
- codex `DEFAULT_OSS_MODEL`:
  `codex-rs/ollama/src/lib.rs:17`

### anie sites

- Current substring heuristic:
  `crates/anie-providers-builtin/src/local.rs:52-57`
- Mirror in discovery:
  `crates/anie-providers-builtin/src/model_discovery.rs:712-732`
- Reasoning strategy resolver:
  `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs:107-170`
- Error classifier extension point:
  `crates/anie-providers-builtin/src/openai/reasoning_strategy.rs:201-220`
- Retry loop:
  `crates/anie-providers-builtin/src/openai/mod.rs:275-300`
- Streaming reasoning extractor (Symptom 2 origin):
  `crates/anie-providers-builtin/src/openai/streaming.rs:119-148`
- `ModelInfo` definition:
  `crates/anie-provider/src/model.rs:224-254`
- `ModelInfo::to_model`:
  `crates/anie-provider/src/model.rs:332-365`
- Ollama `/api/show` example response:

  ```bash
  $ curl -s -X POST http://localhost:11434/api/show \
      -d '{"name":"qwen3.5:9b"}' | jq .capabilities
  ["completion", "vision", "tools", "thinking"]
  ```
