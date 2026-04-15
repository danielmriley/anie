# Phase 2: Providers (Weeks 3–4)

**Goal:** Replace the mock provider with the real integrations required for v1.0. The non-negotiable path is OpenAI-compatible streaming plus Ollama / LM Studio support so you can build and test locally without paying hosted API costs. Anthropic remains the primary hosted provider target. Google is optional if schedule allows. GitHub Copilot OAuth is explicitly post-v1.0.

**v1.0 scope guard:**
- **Required:** 2.1 HTTP/SSE infrastructure, 2.3 OpenAI-compatible provider, 2.6 auth/request resolution, 2.7 config, 2.8 CLI harness, 2.10 Ollama/LM Studio integration.
- **Strongly desired:** 2.2 Anthropic provider.
- **Optional stretch:** 2.4 Google provider.
- **Post-v1.0:** 2.9 GitHub Copilot OAuth.

**Recommended implementation order inside this phase:**
1. 2.1 HTTP/SSE infrastructure
2. 2.3 OpenAI-compatible provider
3. 2.10 Ollama / LM Studio integration
4. 2.6 async request-option resolution
5. 2.7 config
6. 2.8 CLI harness
7. 2.2 Anthropic provider
8. 2.4 Google provider (optional)
9. 2.9 Copilot OAuth (post-v1.0)

---

## Sub-phase 2.1: HTTP and SSE Infrastructure

**Duration:** Days 1–2

Before implementing any provider, build the shared HTTP/SSE machinery they all need.

### HTTP Client Setup

Create a shared `reqwest::Client` factory in `anie-providers-builtin`:

```rust
pub fn create_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300)) // 5-minute request timeout
        .connect_timeout(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .expect("Failed to create HTTP client")
}
```

**TLS considerations:**
- `reqwest` uses `rustls` by default with the `rustls-tls` feature. Use `rustls-tls-native-roots` to trust the system's CA store. This is important for corporate proxies and enterprise environments.
- Consider supporting `SSL_CERT_FILE` or `ANIE_CA_CERTIFICATE` env vars for custom CA injection (like Codex does). Can defer to Phase 6 if needed.

### SSE Stream Parsing

Use `eventsource-stream` crate to parse SSE. Create a helper that converts a `reqwest::Response` into a typed event stream:

```rust
use eventsource_stream::Eventsource;
use futures::StreamExt;

pub async fn sse_stream(
    response: reqwest::Response,
) -> impl Stream<Item = Result<SseEvent, SseError>> {
    let byte_stream = response.bytes_stream();
    byte_stream
        .eventsource()
        .map(|result| {
            result.map(|event| SseEvent {
                event_type: event.event,
                data: event.data,
            })
            .map_err(SseError::from)
        })
}

pub struct SseEvent {
    pub event_type: String,
    pub data: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SseError {
    #[error("Stream error: {0}")]
    Stream(String),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
}
```

### Error Classification Helper

All three providers need to classify HTTP status codes. Build a shared helper:

```rust
pub fn classify_http_error(status: StatusCode, body: &str) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::Auth(body.to_string()),
        429 => {
            // Try to extract retry-after from the body (provider-specific)
            ProviderError::RateLimited { retry_after_ms: None }
        }
        529 => ProviderError::RateLimited { retry_after_ms: None }, // Anthropic overloaded
        400 if body.contains("context") || body.contains("token") => {
            ProviderError::ContextOverflow(body.to_string())
        }
        _ => ProviderError::Http {
            status: status.as_u16(),
            body: body.to_string(),
        },
    }
}
```

### Acceptance Criteria

- `create_http_client()` builds successfully.
- `sse_stream()` parses a mock SSE response into events.
- Error classification covers 401, 403, 429, 529, and context overflow.

---

## Sub-phase 2.2: Anthropic Provider

**Duration:** Days 2–5

Anthropic has the cleanest hosted-provider SSE protocol and is the recommended cloud implementation once the OpenAI-compatible local path is working.

### Wire Format

**Request:** `POST /v1/messages`

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 8192,
  "stream": true,
  "system": [
    {
      "type": "text",
      "text": "You are an expert coding assistant...",
      "cache_control": { "type": "ephemeral" }
    }
  ],
  "tools": [
    {
      "name": "read",
      "description": "Read file contents...",
      "input_schema": { ... },
      "cache_control": { "type": "ephemeral" }
    }
  ],
  "messages": [ ... ]
}
```

**Headers:**
```
x-api-key: <key>
anthropic-version: 2023-06-01
content-type: application/json
anthropic-beta: interleaved-thinking-2025-05-14
```

### Message Conversion (`convert_messages`)

Transform `Message[]` → Anthropic's native format:

| Protocol Type | Anthropic Role | Content Mapping |
|---|---|---|
| `UserMessage` | `user` | `ContentBlock::Text` → `{ type: "text", text }` |
| `AssistantMessage` | `assistant` | Text → `{ type: "text", text }`, Thinking → `{ type: "thinking", thinking }`, ToolCall → `{ type: "tool_use", id, name, input }` |
| `ToolResultMessage` | `user` | `{ type: "tool_result", tool_use_id, content: [...] }` |

**Critical detail — tool results in Anthropic:**
Anthropic expects tool results as part of a `user` message, not as a separate role. Multiple consecutive `ToolResultMessage`s must be merged into a single `user` message with multiple `tool_result` content blocks.

```rust
fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
    let mut result = Vec::new();
    let mut pending_tool_results: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg {
            Message::ToolResult(tr) => {
                pending_tool_results.push(self.tool_result_to_content(tr));
            }
            _ => {
                // Flush pending tool results first
                if !pending_tool_results.is_empty() {
                    result.push(LlmMessage {
                        role: "user".into(),
                        content: serde_json::Value::Array(
                            std::mem::take(&mut pending_tool_results)
                        ),
                    });
                }
                // Convert the current message
                result.push(self.convert_single(msg));
            }
        }
    }

    // Flush any remaining tool results
    if !pending_tool_results.is_empty() {
        result.push(LlmMessage {
            role: "user".into(),
            content: serde_json::Value::Array(pending_tool_results),
        });
    }

    result
}
```

### Tool Conversion (`convert_tools`)

```rust
fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
    tools.iter().map(|t| {
        json!({
            "name": t.name,
            "description": t.description,
            "input_schema": t.parameters,
            "cache_control": { "type": "ephemeral" }
        })
    }).collect()
}
```

The `cache_control` field enables Anthropic's prompt caching. The system prompt and tool definitions are marked as cacheable.

### SSE Event Handling

Anthropic SSE events:

| Event Type | Data | Mapped To |
|---|---|---|
| `message_start` | `{ message: { ... } }` | `ProviderEvent::Start` |
| `content_block_start` | `{ content_block: { type: "text" | "tool_use" | "thinking" } }` | `TextStart` / `ThinkingStart` / `ToolCallStart` |
| `content_block_delta` | `{ delta: { type: "text_delta", text } }` | `TextDelta` / `ThinkingDelta` / `ToolCallDelta` |
| `content_block_stop` | `{ index }` | `TextEnd` / `ThinkingEnd` / `ToolCallEnd` |
| `message_delta` | `{ delta: { stop_reason } }` | (accumulate) |
| `message_stop` | `{}` | `ProviderEvent::Done(assembled_message)` |
| `error` | `{ error: { message } }` | `Err(ProviderError::Stream(...))` |

**Stream implementation:**

```rust
impl Provider for AnthropicProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let client = self.client.clone();
        let url = format!("{}/v1/messages", model.base_url);
        let body = self.build_request_body(model, &context, &options);

        let stream = async_stream::try_stream! {
            let mut request = client.post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");

            if let Some(api_key) = &options.api_key {
                request = request.header("x-api-key", api_key);
            }

            let response = request
                .json(&body)
                .send()
                .await
                .map_err(|e| ProviderError::Request(e.to_string()))?;

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                Err(classify_http_error(status, &body))?;
            }

            let mut sse = sse_stream(response).await;
            let mut builder = AnthropicMessageBuilder::new();

            while let Some(event) = sse.next().await {
                match event {
                    Ok(sse_event) => {
                        for provider_event in builder.process_sse(sse_event) {
                            yield provider_event;
                        }
                    }
                    Err(error) => Err(ProviderError::Stream(error.to_string()))?,
                }
            }
        };

        Ok(Box::pin(stream))
    }
}
```

### Thinking / Reasoning Support

Map `ThinkingLevel` to Anthropic's `thinking.budget_tokens`:

```rust
fn thinking_config(thinking: ThinkingLevel, max_tokens: u64) -> Option<serde_json::Value> {
    match thinking {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some(json!({
            "type": "enabled",
            "budget_tokens": (max_tokens / 4).max(1024)
        })),
        ThinkingLevel::Medium => Some(json!({
            "type": "enabled",
            "budget_tokens": (max_tokens / 2).max(2048)
        })),
        ThinkingLevel::High => Some(json!({
            "type": "enabled",
            "budget_tokens": max_tokens
        })),
    }
}
```

When thinking is enabled:
- Set `temperature` to `1` (required by Anthropic for extended thinking).
- Add `anthropic-beta: interleaved-thinking-2025-05-14` header.
- Increase `max_tokens` to accommodate both thinking and output.

### Usage Extraction

Parse `message_start` and `message_delta` for usage data:

```rust
fn extract_usage(data: &serde_json::Value) -> Usage {
    Usage {
        input_tokens: data["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: data["usage"]["output_tokens"].as_u64().unwrap_or(0),
        cache_read_tokens: data["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: data["usage"]["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        ..Default::default()
    }
}
```

### Tests

1. **Message conversion:** Test all message types including tool result batching.
2. **Tool conversion:** Verify `cache_control` is added.
3. **SSE parsing:** Feed sample Anthropic SSE events, verify `ProviderEvent` output.
4. **Thinking config:** Test all `ThinkingLevel` variants.
5. **Live API test (optional, gated by env var):** Send a simple prompt to the Anthropic API.

```rust
#[tokio::test]
#[ignore] // Only run with ANTHROPIC_API_KEY set
async fn test_anthropic_live() {
    let key = std::env::var("ANTHROPIC_API_KEY").unwrap();
    // ... send "say hello" prompt, verify response
}
```

### Acceptance Criteria

- Anthropic provider sends requests and parses SSE responses correctly.
- Tool calls are extracted from SSE events.
- Thinking content is captured when enabled.
- Usage/cost data is reported.

---

## Sub-phase 2.3: OpenAI-Compatible Provider

**Duration:** Days 5–7

### Wire Format

**Request:** `POST /v1/chat/completions`

```json
{
  "model": "gpt-4o",
  "stream": true,
  "stream_options": { "include_usage": true },
  "messages": [ ... ],
  "tools": [ ... ],
  "temperature": 0.7,
  "max_completion_tokens": 8192
}
```

**Headers:**
```
Authorization: Bearer <key>
Content-Type: application/json
```

### Message Conversion

| Protocol Type | OpenAI Role | Content Mapping |
|---|---|---|
| `UserMessage` | `user` | Text → `{ type: "text", text }` |
| `AssistantMessage` | `assistant` | Text → `content` string, ToolCalls → `tool_calls` array |
| `ToolResultMessage` | `tool` | `{ role: "tool", tool_call_id, content: "..." }` |

**Key difference from Anthropic:** OpenAI uses a dedicated `tool` role for tool results, not `user`.

**Tool calls in assistant messages:**
OpenAI puts tool calls in a separate `tool_calls` array, not in `content`:

```json
{
  "role": "assistant",
  "content": "I'll read the file.",
  "tool_calls": [
    {
      "id": "call_abc123",
      "type": "function",
      "function": { "name": "read", "arguments": "{\"path\": \"src/main.rs\"}" }
    }
  ]
}
```

**Critical detail — arguments are strings:**
OpenAI sends `function.arguments` as a JSON-encoded *string*, not a parsed object. The stream sends argument chunks as string fragments. The provider must:
1. Accumulate argument string fragments during streaming.
2. Parse the final string as JSON.
3. Store it as `serde_json::Value` in the `ToolCall`.

### SSE Event Handling

OpenAI SSE format:

```
data: {"id":"chatcmpl-...","choices":[{"delta":{"role":"assistant","content":"Hello"},"index":0}]}

data: {"id":"chatcmpl-...","choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_abc","function":{"name":"read","arguments":"{\"pa"}}]},"index":0}]}

data: {"id":"chatcmpl-...","usage":{"prompt_tokens":100,"completion_tokens":50}}

data: [DONE]
```

Map these to `ProviderEvent`:
- `delta.content` → `ProviderEvent::TextDelta`
- `delta.tool_calls[i].function.name` → `ProviderEvent::ToolCallStart`
- `delta.tool_calls[i].function.arguments` → `ProviderEvent::ToolCallDelta`
- `finish_reason: "tool_calls"` → `StopReason::ToolUse`
- `finish_reason: "stop"` → `StopReason::Stop`
- `[DONE]` → `ProviderEvent::Done`

### Reasoning Support (o-series)

For o-series models (o1, o3, o4-mini), map `ThinkingLevel` to `reasoning_effort`:

```rust
fn reasoning_effort(thinking: ThinkingLevel) -> Option<&'static str> {
    match thinking {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High => Some("high"),
    }
}
```

Add to request body when the model supports reasoning:
```json
{
  "reasoning_effort": "medium",
  "reasoning": { "summary": "auto" }
}
```

**Planning note for local models:** this hosted OpenAI/o-series mapping is only the simple case. Local OpenAI-compatible servers are more varied: many modern local backends can now accept native OpenAI-compatible reasoning controls, but tagged reasoning output is still common and often coexists with those native controls. That work is intentionally planned separately in:
- `docs/local_model_thinking_plan.md`
- `docs/phased_plan_v1-0-1/`

### OpenAI-Compatible Endpoints

This is the critical v1.0 provider because the same implementation works for:
- OpenAI itself
- Ollama
- LM Studio
- local `vllm`
- hosted OpenAI-compatible APIs (Together, Groq, OpenRouter-compatible gateways, etc.)

Changing `base_url` is enough; no provider-specific code path is required.

### Tests

1. **Message conversion:** User, assistant (with tool calls), tool result.
2. **Argument string parsing:** Stream argument fragments, verify JSON parsing.
3. **SSE parsing:** Sample OpenAI SSE events → `ProviderEvent`.
4. **Reasoning effort mapping.**
5. **Live test:** GPT-4o simple prompt (gated by env var).

### Acceptance Criteria

- OpenAI provider handles all message types.
- Tool call argument string fragments are accumulated and parsed correctly.
- Compatible with OpenAI-compatible endpoints via `base_url`.

---

## Sub-phase 2.4: Google Provider (Optional Stretch)

**Duration:** Days 7–8

### Wire Format

**Request:** `POST /v1beta/models/{model}:streamGenerateContent?alt=sse`

```json
{
  "systemInstruction": {
    "parts": [{ "text": "..." }]
  },
  "contents": [ ... ],
  "tools": [{
    "functionDeclarations": [ ... ]
  }],
  "generationConfig": {
    "maxOutputTokens": 8192,
    "temperature": 0.7,
    "thinkingConfig": {
      "thinkingBudget": 4096
    }
  }
}
```

**Headers:**
```
x-goog-api-key: <key>
Content-Type: application/json
```

### Message Conversion

Google uses `contents` with `role: "user"` / `"model"`:

| Protocol Type | Google Role | Content Mapping |
|---|---|---|
| `UserMessage` | `user` | `{ parts: [{ text }] }` |
| `AssistantMessage` | `model` | `{ parts: [{ text }, { functionCall: { name, args } }] }` |
| `ToolResultMessage` | `user` | `{ parts: [{ functionResponse: { name, response } }] }` |

**Tool result batching:** Like Anthropic, Google requires consecutive tool results to be batched into a single `user` turn.

### Tool Conversion

```rust
fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
    vec![json!({
        "functionDeclarations": tools.iter().map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.parameters,
            })
        }).collect::<Vec<_>>()
    })]
}
```

### SSE Event Handling

Google SSE returns `data: { "candidates": [...] }` objects:

```
data: {"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"},"safetyRatings":[...]}],"usageMetadata":{...}}
```

Map:
- `parts[i].text` → `ProviderEvent::TextDelta`
- `parts[i].thought: true` + `parts[i].text` → `ProviderEvent::ThinkingDelta`
- `parts[i].functionCall` → `ProviderEvent::ToolCallStart` + `ProviderEvent::ToolCallEnd` (Google sends complete function calls, not streaming fragments)
- `finishReason: "STOP"` → `StopReason::Stop`
- `finishReason: "TOOL_USE"` → `StopReason::ToolUse` (actually reported as separate function call parts)

**Key difference:** Google does not stream tool call arguments incrementally. The entire `functionCall` arrives in one chunk. This simplifies parsing.

### Thinking Support

```rust
fn thinking_config(thinking: ThinkingLevel) -> Option<serde_json::Value> {
    match thinking {
        ThinkingLevel::Off => Some(json!({ "thinkingBudget": 0 })),
        ThinkingLevel::Low => Some(json!({ "thinkingBudget": 2048 })),
        ThinkingLevel::Medium => Some(json!({ "thinkingBudget": 8192 })),
        ThinkingLevel::High => Some(json!({ "thinkingBudget": 32768 })),
    }
}
```

### Tests

1. **Message conversion** with tool result batching.
2. **SSE parsing** with complete function calls.
3. **Thinking config** mapping.
4. **Live test** with Gemini 2.5 Pro (gated by env var).

### Acceptance Criteria

- Google provider sends requests and parses responses.
- Function calls are extracted correctly.
- Thinking content is captured from thought-flagged parts.

---

## Sub-phase 2.5: Provider Registration and Model Catalog

**Duration:** Day 8

### Registration Function

```rust
// crates/anie-providers-builtin/src/lib.rs

pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register(ApiKind::AnthropicMessages, Box::new(AnthropicProvider::new()));
    registry.register(ApiKind::OpenAICompletions, Box::new(OpenAIProvider::new()));
    registry.register(ApiKind::GoogleGenerativeAI, Box::new(GoogleProvider::new()));
}
```

### Built-in Model Catalog

Create a static model list. Unlike pi (which generates from provider APIs), start with a hardcoded list that covers the most common models:

```rust
pub fn builtin_models() -> Vec<Model> {
    vec![
        // Anthropic
        Model {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            provider: "anthropic".into(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            supports_reasoning: true,
            supports_images: true,
            cost_per_million: CostPerMillion { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 },
        },
        Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            // ...
        },
        // OpenAI
        Model {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com".into(),
            context_window: 128_000,
            max_tokens: 16_384,
            supports_reasoning: false,
            supports_images: true,
            cost_per_million: CostPerMillion { input: 2.5, output: 10.0, cache_read: 1.25, cache_write: 0.0 },
        },
        Model {
            id: "o4-mini".into(),
            name: "o4-mini".into(),
            // ...
            supports_reasoning: true,
        },
        // Google
        Model {
            id: "gemini-2.5-pro".into(),
            name: "Gemini 2.5 Pro".into(),
            provider: "google".into(),
            api: ApiKind::GoogleGenerativeAI,
            base_url: "https://generativelanguage.googleapis.com".into(),
            context_window: 1_048_576,
            max_tokens: 65_536,
            supports_reasoning: true,
            supports_images: true,
            cost_per_million: CostPerMillion { input: 1.25, output: 10.0, cache_read: 0.315, cache_write: 0.0 },
        },
    ]
}
```

### Acceptance Criteria

- All three providers are registered and callable via `ProviderRegistry`.
- Model catalog includes at least 6 models across 3 providers.

---

## Sub-phase 2.6: `anie-auth` — Async Request Option Resolution

**Duration:** Days 9–10

Implement the minimal auth layer needed for v1: API keys via env vars and `auth.json`.

### Auth Storage (`auth.json`)

```rust
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(flatten)]
    pub providers: HashMap<String, AuthCredential>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthCredential {
    #[serde(rename = "api_key")]
    ApiKey { key: String },
    // OAuth variant reserved for v2
}
```

**File location:** `~/.anie/auth.json`
**Permissions:** Mode `0600` on creation. Warn if permissions are too open.

```rust
pub fn load_auth_store() -> Result<AuthStore> {
    let path = auth_file_path();
    if !path.exists() {
        return Ok(AuthStore { providers: HashMap::new() });
    }

    // Check permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)?.permissions().mode();
        if mode & 0o077 != 0 {
            tracing::warn!(
                "~/.anie/auth.json has overly permissive mode {:o}. Consider chmod 600.",
                mode & 0o777
            );
        }
    }

    let content = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&content)?)
}
```

### Request Options Resolution

Implement the priority chain as an **async** resolver that returns `ResolvedRequestOptions`, not just a bare string. This avoids a later architecture break when providers need per-request headers or `base_url` overrides.

```rust
pub struct AuthResolver {
    pub cli_api_key: Option<String>,
    pub config: AnieConfig,
}

#[async_trait]
impl RequestOptionsResolver for AuthResolver {
    async fn resolve(
        &self,
        model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        // 1. CLI flag override
        let api_key = if let Some(key) = &self.cli_api_key {
            Some(key.clone())
        }
        // 2. auth.json
        else if let Ok(store) = load_auth_store() {
            match store.providers.get(&model.provider) {
                Some(AuthCredential::ApiKey { key }) => Some(key.clone()),
                None => None,
            }
        } else {
            None
        };

        // 3. Provider-configured env var, then built-in fallback
        let api_key = api_key.or_else(|| {
            let configured_env = self.config.providers
                .get(&model.provider)
                .and_then(|p| p.api_key_env.as_deref());
            let env_var = configured_env.or_else(|| match model.provider.as_str() {
                "anthropic" => Some("ANTHROPIC_API_KEY"),
                "openai" => Some("OPENAI_API_KEY"),
                "google" => Some("GEMINI_API_KEY"),
                _ => None,
            })?;
            std::env::var(env_var).ok()
        });

        Ok(ResolvedRequestOptions {
            api_key,
            headers: HashMap::new(),
            base_url_override: None,
        })
    }
}
```

Local OpenAI-compatible servers are allowed to resolve to `api_key: None`.

### Save Key

```rust
pub fn save_api_key(provider: &str, key: &str) -> Result<()> {
    let path = auth_file_path();
    let mut store = load_auth_store().unwrap_or_default();
    store.providers.insert(
        provider.to_string(),
        AuthCredential::ApiKey { key: key.to_string() },
    );

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(&store)?;
    std::fs::write(&path, json)?;

    // Set restrictive permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}
```

### Acceptance Criteria

- API keys can be saved and loaded from `auth.json`.
- Request options resolve asynchronously with priority: CLI → auth.json → env var.
- Resolver can legally return `api_key: None` for local OpenAI-compatible models.
- File created with mode 0600 on Unix.

---

## Sub-phase 2.7: `anie-config` — TOML Configuration

**Duration:** Days 10–11

### Config Structure

```rust
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AnieConfig {
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub context: ContextConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_model_id")]
    pub id: String,
    #[serde(default = "default_thinking")]
    pub thinking: ThinkingLevel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub api: Option<ApiKind>,
    pub models: Option<Vec<CustomModelConfig>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_reserve_tokens")]
    pub reserve_tokens: u64,  // 16384
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: u64,  // 20000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_context_filenames")]
    pub filenames: Vec<String>,  // ["AGENTS.md", "CLAUDE.md"]
    #[serde(default = "default_context_max_file_bytes")]
    pub max_file_bytes: u64,     // 32768
    #[serde(default = "default_context_max_total_bytes")]
    pub max_total_bytes: u64,    // 65536
}
```

### Layer Merging

```rust
pub fn load_config(cli_overrides: CliOverrides) -> Result<AnieConfig> {
    // Layer 1: Global
    let global_path = dirs::home_dir().unwrap().join(".anie/config.toml");
    let mut config = load_toml_or_default(&global_path)?;

    // Layer 2: Project
    let project_path = find_project_config()?; // walk CWD upward for .anie/config.toml
    if let Some(project_config) = project_path {
        let project = load_toml_or_default(&project_config)?;
        merge_config(&mut config, &project);
    }

    // Layer 3: CLI overrides
    apply_cli_overrides(&mut config, &cli_overrides);

    Ok(config)
}
```

**Merge strategy:** Field-by-field, later non-`None` wins. For nested structures, merge recursively. For `providers` maps, merge by key.

### Default Config File

When `~/.anie/config.toml` doesn't exist, create it with commented defaults:

```toml
# anie-rs configuration
# See: https://github.com/example/anie-rs#configuration

# Default model
# [model]
# provider = "anthropic"
# id = "claude-sonnet-4-6"
# thinking = "medium"

# Provider settings
# [providers.anthropic]
# api_key_env = "ANTHROPIC_API_KEY"

# Compaction settings
# [compaction]
# enabled = true
# reserve_tokens = 16384
# keep_recent_tokens = 20000

# Project context files
# [context]
# filenames = ["AGENTS.md", "CLAUDE.md"]
# max_file_bytes = 32768
# max_total_bytes = 65536
```

### Tests

1. Parse a minimal TOML config.
2. Parse a full config with all fields.
3. Layer merging: global + project + CLI.
4. Default values when fields are omitted.
5. Custom provider with models.

### Acceptance Criteria

- Config loads from TOML with correct defaults.
- Three-layer merging works correctly.
- Custom providers with models can be defined.
- Project-context caps prevent oversized `AGENTS.md` / `CLAUDE.md` files from consuming the entire prompt.

---

## Sub-phase 2.8: CLI Test Harness

**Duration:** Days 11–12

Before building the TUI (Phase 3), create a minimal CLI harness for end-to-end testing against real APIs. This is a temporary tool in `anie-cli` that will be replaced by the full TUI later.

```rust
// Temporary test harness — not the final CLI
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::init();

    let config = load_config(CliOverrides::default())?;

    let mut provider_registry = ProviderRegistry::new();
    register_builtin_providers(&mut provider_registry);

    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Arc::new(ReadTool::new(".")));
    tool_registry.register(Arc::new(WriteTool::new(".")));
    tool_registry.register(Arc::new(BashTool::new(".")));

    let model = find_model(&config)?; // Prefer a local Ollama model in default config for zero-cost testing
    let request_options_resolver = Arc::new(AuthResolver {
        cli_api_key: None,
        config: config.clone(),
    });

    let agent = AgentLoop::new(
        Arc::new(provider_registry),
        Arc::new(tool_registry),
        AgentLoopConfig {
            model: model.clone(),
            system_prompt: "You are a helpful coding assistant.".into(),
            thinking: config.model.thinking,
            tool_execution: ToolExecutionMode::Parallel,
            request_options_resolver,
            get_steering_messages: None,
            get_follow_up_messages: None,
        },
    );

    let prompt = std::env::args().nth(1).unwrap_or("say hello".into());
    let prompts = vec![Message::User(UserMessage {
        content: vec![ContentBlock::Text { text: prompt }],
        timestamp: now_millis(),
    })];

    let (event_tx, mut event_rx) = mpsc::channel(100);
    let cancel = CancellationToken::new();

    let handle = tokio::spawn(async move {
        agent.run(prompts, Vec::new(), event_tx, cancel).await
    });

    while let Some(event) = event_rx.recv().await {
        match &event {
            AgentEvent::MessageDelta { delta: StreamDelta::TextDelta(text) } => {
                print!("{}", text);
                std::io::stdout().flush()?;
            }
            AgentEvent::ToolExecStart { tool_name, .. } => {
                eprintln!("\n[tool: {}]", tool_name);
            }
            AgentEvent::AgentEnd { .. } => break,
            _ => {}
        }
    }

    let run_result = handle.await?;
    eprintln!("\n\n[Done. {} generated messages]", run_result.generated_messages.len());
    Ok(())
}
```

### Acceptance Criteria

- `cargo run -- "read the file Cargo.toml and tell me the crate names"` works against a real provider.
- At least one zero-cost local path works end-to-end (`ollama` or `lmstudio`).
- Tool calls are executed and results are fed back to the model.
- Streaming text output is visible.

---

## Sub-phase 2.9: Post-v1.0 — GitHub Copilot OAuth (Device Code Flow)

**Duration:** Days 12–14

GitHub Copilot gives access to 24 models (Claude, GPT, Gemini, Grok) at zero marginal cost through a Copilot subscription. The models are proxied through Copilot's servers using the **same wire formats** (Anthropic Messages, OpenAI Completions) that we already implement — they just need Copilot-specific auth and headers.

### How Copilot Auth Works

Copilot uses the **OAuth device code flow**, which is the simplest OAuth variant — no callback server, no PKCE, no browser redirect to catch.

```
1. POST https://github.com/login/device/code
   Body: client_id=<CLIENT_ID>&scope=read:user
   → Returns: { device_code, user_code, verification_uri, interval, expires_in }

2. Display to user:
   "Go to https://github.com/login/device and enter code: ABCD-1234"

3. Poll https://github.com/login/oauth/access_token every `interval` seconds
   Body: client_id=<CLIENT_ID>&device_code=<code>&grant_type=urn:ietf:params:oauth:grant-type:device_code
   → Eventually returns: { access_token }
   → Until then returns: { error: "authorization_pending" }

4. Exchange GitHub access token for Copilot session token:
   GET https://api.github.com/copilot_internal/v2/token
   Authorization: Bearer <github_access_token>
   → Returns: { token, expires_at }

5. Use the Copilot token as Bearer token for API calls
   The token itself contains the API base URL: proxy-ep=proxy.individual.githubcopilot.com
   Convert to: https://api.individual.githubcopilot.com
```

### Implementation

Add to `anie-auth`:

```rust
// crates/anie-auth/src/copilot.rs

const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Start the device code flow. Returns the code the user must enter.
pub async fn start_device_flow(
    domain: &str, // "github.com" or enterprise domain
) -> Result<DeviceCodeResponse> {
    let url = format!("https://{}/login/device/code", domain);
    let client = reqwest::Client::new();
    let resp = client.post(&url)
        .header("Accept", "application/json")
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .form(&[
            ("client_id", COPILOT_CLIENT_ID),
            ("scope", "read:user"),
        ])
        .send().await?
        .json::<serde_json::Value>().await?;
    // Parse and return DeviceCodeResponse
    Ok(parse_device_code_response(&resp)?)
}

/// Poll until the user authorizes, or timeout.
pub async fn poll_for_access_token(
    domain: &str,
    device_code: &str,
    interval_secs: u64,
    expires_in: u64,
    cancel: &CancellationToken,
) -> Result<String> {
    let url = format!("https://{}/login/oauth/access_token", domain);
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let mut interval = Duration::from_secs(interval_secs.max(1));

    loop {
        if cancel.is_cancelled() {
            anyhow::bail!("Login cancelled");
        }
        if Instant::now() > deadline {
            anyhow::bail!("Device flow timed out");
        }

        tokio::time::sleep(interval).await;

        let resp = client.post(&url)
            .header("Accept", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .form(&[
                ("client_id", COPILOT_CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send().await?
            .json::<serde_json::Value>().await?;

        if let Some(token) = resp.get("access_token").and_then(|v| v.as_str()) {
            return Ok(token.to_string());
        }

        match resp.get("error").and_then(|v| v.as_str()) {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                // Increase interval by 5 seconds
                interval += Duration::from_secs(5);
                continue;
            }
            Some(error) => anyhow::bail!("Device flow failed: {}", error),
            None => anyhow::bail!("Unexpected response: {}", resp),
        }
    }
}

/// Exchange GitHub access token for a Copilot session token.
pub async fn get_copilot_token(
    github_token: &str,
    domain: &str,
) -> Result<CopilotToken> {
    let url = format!("https://api.{}/copilot_internal/v2/token", domain);
    let client = reqwest::Client::new();
    let resp = client.get(&url)
        .header("Authorization", format!("Bearer {}", github_token))
        .header("User-Agent", "GitHubCopilotChat/0.35.0")
        .header("Editor-Version", "vscode/1.107.0")
        .header("Editor-Plugin-Version", "copilot-chat/0.35.0")
        .header("Copilot-Integration-Id", "vscode-chat")
        .send().await?
        .json::<serde_json::Value>().await?;

    let token = resp["token"].as_str()
        .ok_or_else(|| anyhow!("Missing token"))?;
    let expires_at = resp["expires_at"].as_u64()
        .ok_or_else(|| anyhow!("Missing expires_at"))?;

    Ok(CopilotToken {
        token: token.to_string(),
        expires_at: expires_at * 1000, // Convert to millis
        github_token: github_token.to_string(),
        domain: domain.to_string(),
    })
}

pub struct CopilotToken {
    pub token: String,
    pub expires_at: u64,      // Millis since epoch
    pub github_token: String, // Stored as refresh token
    pub domain: String,
}

impl CopilotToken {
    pub fn is_expired(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        // Refresh 5 minutes before expiry
        now_ms > self.expires_at.saturating_sub(5 * 60 * 1000)
    }

    /// Extract API base URL from the token's proxy-ep field.
    /// Token format: tid=...;exp=...;proxy-ep=proxy.individual.githubcopilot.com;...
    pub fn base_url(&self) -> String {
        if let Some(caps) = self.token.find("proxy-ep=") {
            let start = caps + "proxy-ep=".len();
            let end = self.token[start..].find(';').map_or(
                self.token.len(), |i| start + i
            );
            let proxy_host = &self.token[start..end];
            let api_host = proxy_host.replacen("proxy.", "api.", 1);
            return format!("https://{}", api_host);
        }
        // Fallback
        format!("https://api.individual.githubcopilot.com")
    }

    /// Refresh the Copilot token using the stored GitHub access token.
    pub async fn refresh(&mut self) -> Result<()> {
        let new = get_copilot_token(&self.github_token, &self.domain).await?;
        self.token = new.token;
        self.expires_at = new.expires_at;
        Ok(())
    }
}
```

### Auth Credential Storage

Extend `AuthCredential` in `anie-auth` to support OAuth:

```rust
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AuthCredential {
    #[serde(rename = "api_key")]
    ApiKey { key: String },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,      // Copilot session token
        refresh: String,     // GitHub access token
        expires: u64,        // Millis since epoch
        #[serde(skip_serializing_if = "Option::is_none")]
        domain: Option<String>, // Enterprise domain (None = github.com)
    },
}
```

The post-v1.0 implementation extends `AuthResolver` / `RequestOptionsResolver` with an OAuth branch:

```rust
#[async_trait]
impl RequestOptionsResolver for AuthResolver {
    async fn resolve(
        &self,
        model: &Model,
        _context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError> {
        // ... CLI + API-key logic from v1 ...

        if let Ok(store) = load_auth_store() {
            if let Some(AuthCredential::OAuth { access, refresh, expires, domain }) =
                store.providers.get(&model.provider) {
                let now_ms = now_millis();
                let token = if now_ms < expires.saturating_sub(5 * 60 * 1000) {
                    access.clone()
                } else {
                    let domain_str = domain.as_deref().unwrap_or("github.com");
                    let refreshed = get_copilot_token(refresh, domain_str).await?;
                    save_oauth_credential(&model.provider, &refreshed)?;
                    refreshed.token
                };

                return Ok(ResolvedRequestOptions {
                    api_key: Some(token),
                    headers: copilot_headers(&[]),
                    base_url_override: None, // Filled from the token later if needed
                });
            }
        }

        // ... env var fallback ...
    }
}
```

### Copilot-Specific Headers

All requests to Copilot's proxy need these headers:

```rust
pub fn copilot_headers(messages: &[Message]) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("User-Agent".into(), "GitHubCopilotChat/0.35.0".into());
    headers.insert("Editor-Version".into(), "vscode/1.107.0".into());
    headers.insert("Editor-Plugin-Version".into(), "copilot-chat/0.35.0".into());
    headers.insert("Copilot-Integration-Id".into(), "vscode-chat".into());
    headers.insert("Openai-Intent".into(), "conversation-edits".into());

    // X-Initiator: "user" if last message is from user, "agent" otherwise
    let initiator = match messages.last() {
        Some(Message::User(_)) => "user",
        _ => "agent",
    };
    headers.insert("X-Initiator".into(), initiator.into());

    // Vision header if any message contains images
    let has_images = messages.iter().any(|m| match m {
        Message::User(um) => um.content.iter().any(|c| matches!(c, ContentBlock::Image { .. })),
        Message::ToolResult(tr) => tr.content.iter().any(|c| matches!(c, ContentBlock::Image { .. })),
        _ => false,
    });
    if has_images {
        headers.insert("Copilot-Vision-Request".into(), "true".into());
    }

    headers
}
```

These headers are injected via the existing `StreamOptions.headers` field. No provider changes needed — the caller (agent orchestration layer) sets them when the provider is `github-copilot`.

### Copilot Model Catalog

Copilot models use existing API formats but route through Copilot's proxy:

```rust
// Added to builtin_models() in anie-providers-builtin

// GitHub Copilot models (Anthropic API through Copilot proxy)
Model {
    id: "claude-sonnet-4.5".into(),
    name: "Claude Sonnet 4.5 (Copilot)".into(),
    provider: "github-copilot".into(),
    api: ApiKind::AnthropicMessages, // Same API, different base_url
    base_url: "https://api.individual.githubcopilot.com".into(), // Overridden at runtime from token
    context_window: 200_000,
    max_tokens: 32_000,
    supports_reasoning: true,
    supports_images: true,
    cost_per_million: CostPerMillion::zero(), // Included in subscription
},
// GitHub Copilot models (OpenAI API through Copilot proxy)
Model {
    id: "gpt-4o".into(),
    name: "GPT-4o (Copilot)".into(),
    provider: "github-copilot".into(),
    api: ApiKind::OpenAICompletions,
    base_url: "https://api.individual.githubcopilot.com".into(),
    context_window: 128_000,
    max_tokens: 16_384,
    supports_reasoning: false,
    supports_images: true,
    cost_per_million: CostPerMillion::zero(),
},
// ... more Copilot models
```

**Key insight:** No new provider implementation is needed. Copilot models declare `api: ApiKind::AnthropicMessages` or `api: ApiKind::OpenAICompletions` and get routed to the existing providers. The only differences are the `base_url` (extracted from the Copilot token at runtime) and the extra headers.

### Model Enablement After Login

Some Copilot models require policy acceptance. After successful login, call the enablement endpoint for each model:

```rust
pub async fn enable_copilot_models(
    token: &str,
    base_url: &str,
    on_progress: impl Fn(&str, bool),
) {
    let client = reqwest::Client::new();
    let model_ids = [
        "claude-sonnet-4.5", "claude-opus-4.6", "gemini-2.5-pro",
        "gpt-4o", "grok-code-fast-1", // etc.
    ];

    for model_id in model_ids {
        let url = format!("{}/models/{}/policy", base_url, model_id);
        let result = client.post(&url)
            .header("Authorization", format!("Bearer {}", token))
            .header("Content-Type", "application/json")
            .header("User-Agent", "GitHubCopilotChat/0.35.0")
            .header("openai-intent", "chat-policy")
            .json(&serde_json::json!({ "state": "enabled" }))
            .send().await;
        let success = result.map_or(false, |r| r.status().is_success());
        on_progress(model_id, success);
    }
}
```

### GitHub Enterprise Support

The device flow URLs change for enterprise:
- `github.com` → `company.ghe.com`
- `api.github.com` → `api.company.ghe.com`

The login flow should prompt for an enterprise domain (blank = github.com), matching pi's approach:

```
GitHub Enterprise URL/domain (blank for github.com): _
```

### `/login` Slash Command

Add a `/login copilot` slash command (Phase 5) that runs the device flow inline in the TUI:

```
> /login copilot
GitHub Enterprise URL (blank for github.com): 
Go to https://github.com/login/device and enter code: ABCD-1234
Waiting for authorization... ⠋
✓ Logged in to GitHub Copilot
✓ Enabled 24 models
Credentials saved to ~/.anie/auth.json
```

### Tests

1. **Device code flow (mock):** Mock the GitHub endpoints, verify poll loop handles `authorization_pending` and `slow_down`.
2. **Token refresh:** Verify expired token triggers refresh.
3. **Base URL extraction:** Parse `proxy-ep` from sample Copilot tokens.
4. **Enterprise URLs:** Verify URL construction for enterprise domains.
5. **Copilot headers:** Verify `X-Initiator` and `Copilot-Vision-Request` logic.
6. **Live test (gated):** `GITHUB_COPILOT_TOKEN` env var → send prompt to Copilot.

### Acceptance Criteria

- Device code flow completes and stores OAuth credentials in `auth.json`.
- Copilot token auto-refreshes when expired.
- Copilot models appear in the model catalog and route through existing providers.
- Extra headers are sent on every Copilot request.

---

## Sub-phase 2.10: Ollama and LM Studio (Required for v1.0)

**Duration:** Day 14

Ollama and LM Studio both expose **OpenAI-compatible endpoints**. This means our existing `OpenAIProvider` works with them out of the box — just change `base_url`. This is a required v1.0 capability because it gives you a zero-cost development and test loop. The user can already configure this manually in `config.toml`:

```toml
[providers.ollama]
base_url = "http://localhost:11434/v1"
api = "openai-completions"

[[providers.ollama.models]]
id = "qwen3:72b"
name = "Qwen 3 72B"
context_window = 32768
max_tokens = 8192

[providers.lmstudio]
base_url = "http://localhost:1234/v1"
api = "openai-completions"

[[providers.lmstudio.models]]
id = "loaded-model"
name = "LM Studio Model"
context_window = 32768
max_tokens = 8192
```

This sub-phase adds **auto-detection** so users don't have to write config manually.

**Planning note:** the simple auto-detection path below is enough for the current zero-cost local-provider gate, but it is intentionally conservative about reasoning support. `/v1/models` does not tell us whether a local model/server supports native reasoning controls, native separated reasoning output, tagged thinking output, or only prompt-based reasoning. The follow-on design for that work lives in:
- `docs/local_model_thinking_plan.md`
- `docs/phased_plan_v1-0-1/`

### Auto-Detection

At startup, probe known local endpoints to discover running servers and their models:

```rust
// crates/anie-providers-builtin/src/local.rs

use reqwest::Client;
use std::time::Duration;

/// Detected local LLM server.
pub struct LocalServer {
    pub name: String,       // "ollama" or "lmstudio"
    pub base_url: String,
    pub models: Vec<Model>,
}

/// Probe known local endpoints for running LLM servers.
/// Non-blocking: each probe has a 1-second timeout.
pub async fn detect_local_servers() -> Vec<LocalServer> {
    let client = Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();

    let mut servers = Vec::new();

    // Probe Ollama (default port 11434)
    if let Some(server) = probe_openai_compatible(
        &client,
        "ollama",
        "http://localhost:11434",
    ).await {
        servers.push(server);
    }

    // Probe LM Studio (default port 1234)
    if let Some(server) = probe_openai_compatible(
        &client,
        "lmstudio",
        "http://localhost:1234",
    ).await {
        servers.push(server);
    }

    servers
}

async fn probe_openai_compatible(
    client: &Client,
    name: &str,
    base_url: &str,
) -> Option<LocalServer> {
    // GET /v1/models — standard OpenAI-compatible endpoint
    let url = format!("{}/v1/models", base_url);
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() { return None; }

    let body: serde_json::Value = resp.json().await.ok()?;
    let models_data = body.get("data")?.as_array()?;

    let models: Vec<Model> = models_data.iter().filter_map(|m| {
        let id = m.get("id")?.as_str()?;
        Some(Model {
            id: id.to_string(),
            name: id.to_string(),
            provider: name.to_string(),
            api: ApiKind::OpenAICompletions,
            base_url: format!("{}/v1", base_url),
            // Conservative defaults — local models vary widely
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
        })
    }).collect();

    if models.is_empty() { return None; }

    tracing::info!("Detected {} with {} model(s) at {}", name, models.len(), base_url);
    Some(LocalServer {
        name: name.to_string(),
        base_url: base_url.to_string(),
        models,
    })
}
```

### Ollama-Specific: Model Context Window

Ollama's `/api/show` endpoint returns model metadata including context length. Use it to get accurate context windows:

```rust
async fn get_ollama_context_window(
    client: &Client,
    model_id: &str,
) -> Option<u64> {
    let url = "http://localhost:11434/api/show";
    let resp = client.post(url)
        .json(&serde_json::json!({ "model": model_id }))
        .send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;

    // Ollama returns model info with parameters including num_ctx
    body.pointer("/model_info/context_length")
        .or_else(|| body.pointer("/parameters/num_ctx"))
        .and_then(|v| v.as_u64())
}
```

### Provider Quirks

Local models have some behavioral differences from cloud APIs:

1. **No `usage` field:** Many local servers don't return token usage in the SSE stream. The OpenAI provider must handle missing `usage` gracefully (default to zeros).
2. **No API key required:** `StreamOptions.api_key` may be `None`. The OpenAI provider should skip the `Authorization` header when no key is present.
3. **Slower streaming:** Local models may stream much slower. No special handling needed, but the TUI spinner should remain responsive.
4. **No `stream_options`:** Some servers don't support `{ "stream_options": { "include_usage": true } }`. If the server returns an error mentioning this field, retry without it.

Update the OpenAI provider to handle these:

```rust
// In OpenAI provider's stream() method:

// Skip auth header if no API key
let mut req = client.post(&url).header("Content-Type", "application/json");
if let Some(api_key) = &options.api_key {
    req = req.header("Authorization", format!("Bearer {}", api_key));
}

// Usage defaults to zero if not present in response
let usage = Usage {
    input_tokens: data["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
    output_tokens: data["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    ..Default::default()
};
```

### Integration with Startup

In the CLI/TUI startup sequence, run detection and register discovered models:

```rust
// During provider setup
let mut provider_registry = ProviderRegistry::new();
register_builtin_providers(&mut provider_registry);

// Auto-detect local servers (non-blocking, 1s timeout per probe)
let local_servers = detect_local_servers().await;
for server in &local_servers {
    for model in &server.models {
        // Register each discovered model in the model catalog
        model_catalog.push(model.clone());
    }
}

if !local_servers.is_empty() {
    let names: Vec<_> = local_servers.iter()
        .map(|s| format!("{} ({} models)", s.name, s.models.len()))
        .collect();
    tracing::info!("Local LLM servers detected: {}", names.join(", "));
}
```

### Tests

1. **Probe with mock server:** Start a local HTTP server returning `/v1/models`, verify detection.
2. **Probe timeout:** Verify detection completes quickly when no server is running.
3. **Missing API key:** Verify OpenAI provider works without `Authorization` header.
4. **Missing usage:** Verify provider doesn't error when SSE lacks usage data.

### Acceptance Criteria

- `anie` auto-detects Ollama and LM Studio when they're running.
- Discovered models appear in `/model` list.
- Chat works with local models (no API key, no usage data).
- Manual config in `config.toml` still works (takes priority over auto-detection).
- Detection doesn't slow startup when no local server is running (1s timeout).

---

## Phase 2 Milestones Checklist

| # | Milestone | Verified By |
|---|---|---|
| 1 | SSE infrastructure parses OpenAI-compatible and Anthropic streams | Unit tests |
| 2 | OpenAI-compatible provider handles text + tool-call streaming | Live/local test + unit tests |
| 3 | Ollama / LM Studio work without API key or usage data | Mock server test + manual test |
| 4 | Anthropic provider sends and receives messages | Live test + unit tests |
| 5 | All required v1.0 providers are registered via `register_builtin_providers` | Integration test |
| 6 | Async request option resolution works (CLI → auth.json → env var) | Unit tests |
| 7 | TOML config loads with layer merging + context caps | Unit tests |
| 8 | CLI test harness works end-to-end against a local provider | Manual test |
| 9 | Google provider works (optional stretch) | Live test + unit tests |
| 10 | GitHub Copilot device flow login works (post-v1.0) | Mock test + manual test |
