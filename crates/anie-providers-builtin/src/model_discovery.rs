use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::Arc,
    time::{Duration, Instant},
};

use anie_provider::{ApiKind, ModelInfo, ModelPricing, ProviderError};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;

use crate::{classify_http_error, parse_retry_after};

/// Parameters for a model discovery request against a specific provider endpoint.
#[derive(Debug, Clone)]
pub struct ModelDiscoveryRequest {
    /// Provider name used for display and cache keying.
    pub provider_name: String,
    /// Provider wire protocol.
    pub api: ApiKind,
    /// Endpoint base URL.
    pub base_url: String,
    /// Optional API key for authenticated providers.
    pub api_key: Option<String>,
    /// Optional additional headers.
    pub headers: HashMap<String, String>,
}

impl ModelDiscoveryRequest {
    fn cache_key(&self) -> CacheKey {
        CacheKey {
            provider_name: self.provider_name.to_ascii_lowercase(),
            api: self.api,
            base_url: self.base_url.trim().trim_end_matches('/').to_string(),
            auth_fingerprint: auth_fingerprint(self.api_key.as_deref(), &self.headers),
        }
    }
}

/// Discover models from a provider endpoint.
pub async fn discover_models(
    request: &ModelDiscoveryRequest,
) -> Result<Vec<ModelInfo>, ProviderError> {
    match request.api {
        ApiKind::OpenAICompletions | ApiKind::OpenAIResponses => {
            discover_openai_compatible_models(request).await
        }
        ApiKind::AnthropicMessages => discover_anthropic_models(request).await,
        ApiKind::GoogleGenerativeAI => Err(ProviderError::RequestBuild(
            "model discovery for Google Generative AI is not implemented yet".to_string(),
        )),
        ApiKind::OllamaChatApi => Err(ProviderError::RequestBuild(
            "model discovery for OllamaChatApi is not wired until the native Ollama discovery PR"
                .to_string(),
        )),
    }
}

/// In-memory TTL cache for provider model discovery.
///
/// Plan 06 PR-E: hits return `Arc<[ModelInfo]>` so consumers
/// share the backing allocation across lookups instead of
/// deep-cloning the full discovered catalog on every read.
/// Misses wrap the freshly-discovered Vec in an Arc once.
pub struct ModelDiscoveryCache {
    entries: HashMap<CacheKey, CacheEntry>,
    default_ttl: Duration,
}

impl ModelDiscoveryCache {
    /// Create an empty cache with the given default TTL.
    #[must_use]
    pub fn new(default_ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            default_ttl,
        }
    }

    /// Get fresh models from cache or discover and cache them on a miss.
    pub async fn get_or_discover(
        &mut self,
        request: &ModelDiscoveryRequest,
    ) -> Result<Arc<[ModelInfo]>, ProviderError> {
        let key = request.cache_key();
        if let Some(entry) = self.entries.get(&key)
            && entry.fetched_at.elapsed() < self.default_ttl
        {
            return Ok(Arc::clone(&entry.models));
        }

        let models: Arc<[ModelInfo]> = discover_models(request).await?.into();
        self.entries.insert(
            key,
            CacheEntry {
                models: Arc::clone(&models),
                fetched_at: Instant::now(),
            },
        );
        Ok(models)
    }

    /// Force a fresh discovery and replace any cached entry.
    pub async fn refresh(
        &mut self,
        request: &ModelDiscoveryRequest,
    ) -> Result<Arc<[ModelInfo]>, ProviderError> {
        let key = request.cache_key();
        let models: Arc<[ModelInfo]> = discover_models(request).await?.into();
        self.entries.insert(
            key,
            CacheEntry {
                models: Arc::clone(&models),
                fetched_at: Instant::now(),
            },
        );
        Ok(models)
    }

    /// Remove all cached entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    provider_name: String,
    api: ApiKind,
    base_url: String,
    auth_fingerprint: u64,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    models: Arc<[ModelInfo]>,
    fetched_at: Instant,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelsResponse {
    #[serde(default)]
    data: Vec<OpenAiModelEntry>,
}

#[derive(Debug, Deserialize)]
struct OpenAiModelEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_context_tokens: Option<u64>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    modalities: Option<Vec<String>>,
    #[serde(default)]
    capabilities: Option<ModelCapabilities>,
    // OpenRouter-only fields. All optional so the OpenAI
    // `/models` endpoint (and any other OpenAI-compatible
    // endpoint) keeps parsing.
    #[serde(default)]
    pricing: Option<PricingEntry>,
    #[serde(default)]
    top_provider: Option<TopProviderEntry>,
    #[serde(default)]
    supported_parameters: Option<Vec<String>>,
    #[serde(default)]
    architecture: Option<ArchitectureEntry>,
    // GitHub Copilot fields. Pi's Copilot catalog at
    // `packages/ai/src/models.generated.ts` curates the list
    // by hand; the live `/models` endpoint returns extras
    // (embeddings, completion-only, internal routers) that
    // explode with `unsupported_api_for_model` if you try to
    // chat with them. Both fields are optional so OpenAI /
    // OpenRouter / local stays unaffected.
    #[serde(default)]
    supported_endpoints: Option<Vec<String>>,
    #[serde(rename = "type", default)]
    model_type: Option<String>,
    #[serde(default)]
    model_picker_enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PricingEntry {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
    #[serde(default)]
    request: Option<String>,
    #[serde(default)]
    image: Option<String>,
}

impl PricingEntry {
    fn into_model_pricing(self) -> Option<ModelPricing> {
        let pricing = ModelPricing {
            prompt: self.prompt,
            completion: self.completion,
            request: self.request,
            image: self.image,
        };
        if pricing == ModelPricing::default() {
            None
        } else {
            Some(pricing)
        }
    }
}

#[derive(Debug, Deserialize)]
struct TopProviderEntry {
    #[serde(default)]
    context_length: Option<u64>,
    /// OpenRouter-reported cap on *output* tokens for the routed
    /// upstream. When present, we honor it as `Model.max_tokens`
    /// so reasoning models that naturally emit several thousand
    /// tokens of reasoning before answering don't get clipped by
    /// anie's 8k default.
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ArchitectureEntry {
    #[serde(default)]
    input_modalities: Option<Vec<String>>,
    #[serde(default)]
    output_modalities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelsResponse {
    #[serde(default)]
    data: Vec<AnthropicModelEntry>,
}

#[derive(Debug, Deserialize)]
struct AnthropicModelEntry {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    capabilities: Option<ModelCapabilities>,
}

#[derive(Debug, Deserialize)]
struct ModelCapabilities {
    #[serde(default)]
    vision: Option<bool>,
    #[serde(default)]
    images: Option<bool>,
    #[serde(default)]
    reasoning: Option<bool>,
    /// GitHub Copilot nests context + output limits under
    /// `capabilities.limits.*`. OpenAI + OpenRouter surface
    /// them at the top level (`context_length`,
    /// `max_output_tokens`), so this stays Option and is only
    /// read as a fallback source.
    #[serde(default)]
    limits: Option<CapabilityLimits>,
}

#[derive(Debug, Deserialize)]
struct CapabilityLimits {
    #[serde(default)]
    max_context_window_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    /// Upper bound on input-side tokens. Not used directly for
    /// context_window (which the pipeline treats as input +
    /// output combined), but useful if we later surface a
    /// tighter "prompt-only" cap.
    #[allow(dead_code)]
    #[serde(default)]
    max_prompt_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagsResponse {
    #[serde(default)]
    models: Vec<OllamaTagEntry>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    details: Option<OllamaTagDetails>,
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct OllamaTagDetails {
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    families: Option<Vec<String>>,
    #[serde(default)]
    parameter_size: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    context_window: Option<u64>,
}

async fn discover_openai_compatible_models(
    request: &ModelDiscoveryRequest,
) -> Result<Vec<ModelInfo>, ProviderError> {
    if should_try_ollama_tags(request) {
        match discover_ollama_tags(request).await {
            Ok(models) if !models.is_empty() => return Ok(models),
            Ok(_) => {}
            Err(_) => {}
        }
    }

    let client = discovery_http_client()?;
    // GitHub Copilot exposes the model list at `<base>/models`
    // (no `/v1` segment). Its chat endpoint is also at
    // `<base>/chat/completions`, so the canonical base URL the
    // agent stores has no `/v1`. Every other OpenAI-compatible
    // provider normalizes to `<base>/v1/models`.
    let url = if request.provider_name.eq_ignore_ascii_case("github-copilot") {
        format!("{}/models", request.base_url.trim().trim_end_matches('/'))
    } else {
        format!("{}/models", normalize_openai_base_url(&request.base_url))
    };
    let response = send_request(&client, request, url, AuthStyle::Bearer).await?;
    let body = response
        .json::<OpenAiModelsResponse>()
        .await
        .map_err(|error| {
            ProviderError::InvalidStreamJson(format!(
                "failed to parse OpenAI-compatible model list: {error}"
            ))
        })?;

    let provider_is_openrouter = request.provider_name.eq_ignore_ascii_case("openrouter");
    let provider_is_copilot = request.provider_name.eq_ignore_ascii_case("github-copilot");
    Ok(body
        .data
        .into_iter()
        .filter(|entry| entry.object.as_deref().unwrap_or("model") == "model")
        .filter(|entry| {
            // OpenRouter exposes many non-tool models we can't
            // drive from a coding agent (completion-only models,
            // image-gen, etc.). Filter those out for OpenRouter
            // only so the picker stays useful.
            if !provider_is_openrouter {
                return true;
            }
            entry.supported_parameters.as_deref().is_some_and(|params| {
                params
                    .iter()
                    .any(|param| param.eq_ignore_ascii_case("tools"))
            })
        })
        .filter(|entry| {
            // GitHub Copilot's /models response is a mixed bag:
            // chat models, embedding models, internal routing
            // entries (like `accounts/msft/routers/*`), and
            // completions-only models. Selecting a non-chat
            // one produces
            // `HTTP 400 unsupported_api_for_model` on the first
            // request. Keep only entries that advertise
            // /chat/completions + mark type == "chat" +
            // model_picker_enabled != false.
            if !provider_is_copilot {
                return true;
            }
            let endpoints_ok = entry.supported_endpoints.as_deref().is_some_and(|eps| {
                eps.iter()
                    .any(|ep| ep.eq_ignore_ascii_case("/chat/completions"))
            });
            let type_ok = entry
                .model_type
                .as_deref()
                .map(|t| t.eq_ignore_ascii_case("chat"))
                .unwrap_or(true);
            // model_picker_enabled defaults to true if absent —
            // Copilot uses it as the explicit "show in UI" flag.
            let picker_ok = entry.model_picker_enabled.unwrap_or(true);
            endpoints_ok && type_ok && picker_ok
        })
        .map(|entry| {
            let supports_images = infer_openai_images(
                entry.modalities.as_deref(),
                entry.capabilities.as_ref(),
                entry.architecture.as_ref(),
            );
            let supports_reasoning = infer_reasoning(
                request.provider_name.as_str(),
                &entry.id,
                entry.capabilities.as_ref(),
                entry.supported_parameters.as_deref(),
            );
            let context_length = entry
                .context_length
                .or(entry.context_window)
                .or(entry.max_context_tokens)
                .or(entry.input_token_limit)
                .or_else(|| {
                    entry
                        .top_provider
                        .as_ref()
                        .and_then(|top| top.context_length)
                })
                // GitHub Copilot's /models response nests the
                // real context window under capabilities.limits.
                // Without this the fallback in ModelInfo::to_model
                // kicks in and every Copilot model shows 32.8k
                // (= 2^15, the ancient default) regardless of the
                // 200k Opus / 128k GPT sizes Copilot actually
                // serves.
                .or_else(|| {
                    entry
                        .capabilities
                        .as_ref()
                        .and_then(|caps| caps.limits.as_ref())
                        .and_then(|limits| limits.max_context_window_tokens)
                });
            let max_output_tokens = entry
                .top_provider
                .as_ref()
                .and_then(|top| top.max_completion_tokens)
                .or_else(|| {
                    entry
                        .capabilities
                        .as_ref()
                        .and_then(|caps| caps.limits.as_ref())
                        .and_then(|limits| limits.max_output_tokens)
                });
            ModelInfo {
                id: entry.id.clone(),
                name: entry.name.unwrap_or(entry.id.clone()),
                provider: request.provider_name.clone(),
                context_length,
                max_output_tokens,
                supports_images,
                supports_reasoning,
                pricing: entry.pricing.and_then(PricingEntry::into_model_pricing),
                supported_parameters: entry.supported_parameters,
                provider_capabilities: None,
            }
        })
        .collect())
}

async fn discover_anthropic_models(
    request: &ModelDiscoveryRequest,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let client = discovery_http_client()?;
    let url = format!("{}/v1/models", normalize_root_base_url(&request.base_url));
    let response = send_request(&client, request, url, AuthStyle::Anthropic).await?;
    let body = response
        .json::<AnthropicModelsResponse>()
        .await
        .map_err(|error| {
            ProviderError::InvalidStreamJson(format!(
                "failed to parse Anthropic model list: {error}"
            ))
        })?;

    Ok(body
        .data
        .into_iter()
        .map(|entry| ModelInfo {
            id: entry.id.clone(),
            name: entry
                .display_name
                .or(entry.name)
                .unwrap_or(entry.id.clone()),
            provider: request.provider_name.clone(),
            context_length: entry.context_window.or(entry.input_token_limit),
            max_output_tokens: None,
            supports_images: Some(
                entry
                    .capabilities
                    .as_ref()
                    .and_then(|caps| caps.vision.or(caps.images))
                    .unwrap_or(true),
            ),
            supports_reasoning: Some(
                entry
                    .capabilities
                    .as_ref()
                    .and_then(|caps| caps.reasoning)
                    .unwrap_or(true),
            ),
            pricing: None,
            supported_parameters: None,
            provider_capabilities: None,
        })
        .collect())
}

async fn discover_ollama_tags(
    request: &ModelDiscoveryRequest,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let client = discovery_http_client()?;
    let base_url = normalize_root_base_url(&request.base_url);
    let url = format!("{base_url}/api/tags");
    let response = send_request(&client, request, url, AuthStyle::Bearer).await?;
    let body = response
        .json::<OllamaTagsResponse>()
        .await
        .map_err(|error| {
            ProviderError::InvalidStreamJson(format!("failed to parse Ollama tag list: {error}"))
        })?;

    // Materialize the base entries from `/api/tags` (cheap data:
    // id, family, display name, plus the substring-heuristic
    // fallback for `supports_reasoning` / `supports_images`).
    let bases: Vec<OllamaDiscoveryBase> = body
        .models
        .into_iter()
        .filter_map(|entry| {
            let id = entry.model.or(entry.name)?;
            let heuristic_reasoning = entry
                .details
                .as_ref()
                .and_then(|details| {
                    details.family.as_deref().or_else(|| {
                        details
                            .families
                            .as_ref()
                            .and_then(|families| families.first().map(String::as_str))
                    })
                })
                .is_some_and(reasoning_family)
                || reasoning_family(&id);
            let heuristic_images = entry
                .capabilities
                .as_ref()
                .is_some_and(|caps| caps.iter().any(|cap| cap.eq_ignore_ascii_case("vision")));
            let tags_context_length = entry
                .details
                .as_ref()
                .and_then(|details| details.context_length.or(details.context_window));
            let name = ollama_display_name(&id, entry.details.as_ref());
            Some(OllamaDiscoveryBase {
                id,
                name,
                heuristic_reasoning,
                heuristic_images,
                tags_context_length,
            })
        })
        .collect();

    // Fan out `/api/show` calls in parallel. Each probe is
    // independent — per-model failure falls back to the heuristic
    // without aborting the whole discovery call. This is the
    // authoritative capability source; `/api/tags` doesn't
    // populate `capabilities` in practice (verified against local
    // Ollama), so the show call is the only reliable way to tell
    // thinking-capable models apart from lookalike IDs.
    //
    // See docs/ollama_capability_discovery/README.md PR 3.
    let show_futures = bases
        .iter()
        .map(|base| fetch_ollama_show_capabilities(&client, &base_url, &base.id));
    let show_results = futures::future::join_all(show_futures).await;

    Ok(bases
        .into_iter()
        .zip(show_results)
        .map(|(base, show_result)| {
            let OllamaDiscoveryBase {
                id,
                name,
                heuristic_reasoning,
                heuristic_images,
                tags_context_length,
            } = base;
            let (supports_reasoning, supports_images, provider_capabilities, context_length) =
                match show_result {
                    Ok(Some(show)) => {
                        let caps_lower = show.capabilities.as_ref().map(|caps| {
                            caps.iter()
                                .map(|cap| cap.to_ascii_lowercase())
                                .collect::<Vec<_>>()
                        });
                        let thinking = caps_lower
                            .as_ref()
                            .is_some_and(|caps| caps.iter().any(|cap| cap == "thinking"));
                        let vision = caps_lower
                            .as_ref()
                            .is_some_and(|caps| caps.iter().any(|cap| cap == "vision"));
                        let ctx = show.context_length.or(tags_context_length);
                        (Some(thinking), Some(vision), show.capabilities, ctx)
                    }
                    Ok(None) => (
                        Some(heuristic_reasoning),
                        Some(heuristic_images),
                        None,
                        tags_context_length,
                    ),
                    Err(error) => {
                        tracing::warn!(
                            model = %id,
                            %error,
                            "Ollama /api/show probe failed; falling back to substring heuristic"
                        );
                        (
                            Some(heuristic_reasoning),
                            Some(heuristic_images),
                            None,
                            tags_context_length,
                        )
                    }
                };
            ModelInfo {
                id,
                name,
                provider: request.provider_name.clone(),
                context_length,
                max_output_tokens: None,
                supports_images,
                supports_reasoning,
                pricing: None,
                supported_parameters: None,
                provider_capabilities,
            }
        })
        .collect())
}

/// Intermediate per-model data assembled from `/api/tags` before
/// fanning out `/api/show` probes in `discover_ollama_tags`.
struct OllamaDiscoveryBase {
    id: String,
    name: String,
    heuristic_reasoning: bool,
    heuristic_images: bool,
    tags_context_length: Option<u64>,
}

/// Capability-probe result from Ollama's `/api/show`.
pub(crate) struct OllamaShowData {
    /// Raw capability tokens (e.g. `["completion","thinking","vision"]`).
    pub(crate) capabilities: Option<Vec<String>>,
    /// Architectural context length derived from
    /// `model_info["<arch>.context_length"]`.
    #[allow(dead_code)] // consumed by discover_ollama_tags; local.rs ignores context_length
    pub(crate) context_length: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OllamaShowResponse {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    model_info: Option<HashMap<String, serde_json::Value>>,
}

/// POST `/api/show` for a single model, returning its
/// authoritative capabilities and architectural context length.
///
/// `Ok(None)` means the response parsed but carried no capability
/// data; `Err(_)` means a transport or HTTP-level failure. In
/// either case the caller falls back to the heuristic for that
/// specific model without failing the outer discovery call.
pub(crate) async fn fetch_ollama_show_capabilities(
    client: &reqwest::Client,
    base_url: &str,
    model_id: &str,
) -> Result<Option<OllamaShowData>, ProviderError> {
    let url = format!("{}/api/show", base_url.trim_end_matches('/'));
    let body = serde_json::json!({ "name": model_id });
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| {
            ProviderError::Transport(format!("ollama /api/show request failed: {error}"))
        })?;
    if !response.status().is_success() {
        let status = response.status();
        let retry_after = parse_retry_after(&response);
        let body = response.text().await.unwrap_or_default();
        return Err(classify_http_error(status, &body, retry_after));
    }
    let show: OllamaShowResponse = response.json().await.map_err(|error| {
        ProviderError::InvalidStreamJson(format!("failed to parse ollama /api/show: {error}"))
    })?;

    let context_length = show.model_info.as_ref().and_then(|info| {
        let architecture = info
            .get("general.architecture")
            .and_then(serde_json::Value::as_str)?;
        let key = format!("{architecture}.context_length");
        info.get(&key).and_then(serde_json::Value::as_u64)
    });

    Ok(Some(OllamaShowData {
        capabilities: show.capabilities,
        context_length,
    }))
}

async fn send_request(
    client: &reqwest::Client,
    request: &ModelDiscoveryRequest,
    url: String,
    auth_style: AuthStyle,
) -> Result<reqwest::Response, ProviderError> {
    let mut headers = build_headers(request, auth_style)?;
    let mut req = client.get(url.clone()).headers(headers.clone());
    if let Some(api_key) = request.api_key.as_deref() {
        match auth_style {
            AuthStyle::Bearer => {
                req = req.bearer_auth(api_key);
                headers.remove(AUTHORIZATION);
            }
            AuthStyle::Anthropic => {
                headers.insert(
                    HeaderName::from_static("x-api-key"),
                    HeaderValue::from_str(api_key)
                        .map_err(|error| ProviderError::RequestBuild(error.to_string()))?,
                );
                req = client.get(url).headers(headers);
            }
        }
    }

    let response = req.send().await.map_err(|error| {
        ProviderError::Transport(format!("model discovery request failed: {error}"))
    })?;
    if response.status().is_success() {
        return Ok(response);
    }

    let retry_after = parse_retry_after(&response);
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(classify_http_error(status, &body, retry_after))
}

#[derive(Debug, Clone, Copy)]
enum AuthStyle {
    Bearer,
    Anthropic,
}

fn build_headers(
    request: &ModelDiscoveryRequest,
    auth_style: AuthStyle,
) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            ProviderError::RequestBuild(format!("invalid header name '{name}': {error}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            ProviderError::RequestBuild(format!(
                "invalid header value for '{}': {error}",
                name.as_str()
            ))
        })?;
        headers.insert(name, value);
    }

    if matches!(auth_style, AuthStyle::Anthropic)
        && !headers.contains_key(HeaderName::from_static("anthropic-version"))
    {
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
    }

    Ok(headers)
}

fn discovery_http_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| ProviderError::Transport(format!("failed to create HTTP client: {error}")))
}

fn should_try_ollama_tags(request: &ModelDiscoveryRequest) -> bool {
    request.provider_name.eq_ignore_ascii_case("ollama")
        || normalize_root_base_url(&request.base_url).contains(":11434")
}

fn normalize_openai_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn normalize_root_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    trimmed.trim_end_matches("/v1").to_string()
}

fn infer_openai_images(
    modalities: Option<&[String]>,
    capabilities: Option<&ModelCapabilities>,
    architecture: Option<&ArchitectureEntry>,
) -> Option<bool> {
    let modality_has_image = |modalities: &[String]| {
        modalities.iter().any(|modality| {
            modality.eq_ignore_ascii_case("image") || modality.eq_ignore_ascii_case("vision")
        })
    };

    capabilities
        .and_then(|caps| caps.vision.or(caps.images))
        .or_else(|| modalities.map(modality_has_image))
        .or_else(|| {
            architecture.and_then(|arch| {
                arch.input_modalities
                    .as_deref()
                    .map(modality_has_image)
                    .or_else(|| arch.output_modalities.as_deref().map(modality_has_image))
            })
        })
}

/// Model-id families known to support leveled thinking (Ollama's
/// native `think: "low"|"medium"|"high"`). Kept in sync with
/// `local::REASONING_FAMILIES`.
const REASONING_FAMILIES: &[&str] = &["qwen3", "qwq", "deepseek-r1", "gpt-oss"];

/// True when `id` equals `family`, or starts with `family:` or
/// `family-`. See `local::id_matches_reasoning_family` for the
/// rationale (the substring match used to mis-classify
/// `qwen3.5:9b` as the `qwen3` family).
fn id_matches_reasoning_family(id: &str, family: &str) -> bool {
    if id == family {
        return true;
    }
    match id.strip_prefix(family) {
        Some(rest) => matches!(rest.chars().next(), Some(':' | '-')),
        None => false,
    }
}

fn infer_reasoning(
    provider_name: &str,
    model_id: &str,
    capabilities: Option<&ModelCapabilities>,
    supported_parameters: Option<&[String]>,
) -> Option<bool> {
    capabilities
        .and_then(|caps| caps.reasoning)
        .or_else(|| {
            supported_parameters.map(|params| {
                params.iter().any(|param| {
                    param.eq_ignore_ascii_case("reasoning")
                        || param.eq_ignore_ascii_case("reasoning_effort")
                        || param.eq_ignore_ascii_case("include_reasoning")
                })
            })
        })
        .or_else(|| {
            let provider_name = provider_name.to_ascii_lowercase();
            let model_id = model_id.to_ascii_lowercase();
            let reasoning = model_id.contains("reason")
                || model_id.starts_with('o')
                || REASONING_FAMILIES
                    .iter()
                    .any(|family| id_matches_reasoning_family(&model_id, family))
                || provider_name == "anthropic";
            Some(reasoning)
        })
}

fn reasoning_family(family: &str) -> bool {
    let family = family.to_ascii_lowercase();
    REASONING_FAMILIES
        .iter()
        .any(|canonical| id_matches_reasoning_family(&family, canonical))
}

fn ollama_display_name(id: &str, details: Option<&OllamaTagDetails>) -> String {
    let Some(details) = details else {
        return id.to_string();
    };
    match details.parameter_size.as_deref() {
        Some(size) if !size.is_empty() => format!("{id} ({size})"),
        _ => id.to_string(),
    }
}

fn auth_fingerprint(api_key: Option<&str>, headers: &HashMap<String, String>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    api_key.unwrap_or_default().hash(&mut hasher);

    let mut items = headers.iter().collect::<Vec<_>>();
    items.sort_by(|(left_key, _), (right_key, _)| left_key.cmp(right_key));
    for (key, value) in items {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    #[derive(Debug, Clone)]
    struct RequestRecord {
        headers: HashMap<String, String>,
    }

    #[derive(Debug, Clone)]
    struct MockResponse {
        status: u16,
        body: String,
        content_type: &'static str,
    }

    impl MockResponse {
        fn ok_json(body: &str) -> Self {
            Self {
                status: 200,
                body: body.to_string(),
                content_type: "application/json",
            }
        }

        fn status(status: u16, body: &str) -> Self {
            Self {
                status,
                body: body.to_string(),
                content_type: "text/plain",
            }
        }
    }

    struct MockServer {
        base_url: String,
        requests: Arc<Mutex<Vec<RequestRecord>>>,
    }

    async fn spawn_mock_server(
        handler: impl Fn(String, HashMap<String, String>) -> MockResponse + Send + Sync + 'static,
    ) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address: SocketAddr = listener.local_addr().expect("listener address");
        let requests = Arc::new(Mutex::new(Vec::<RequestRecord>::new()));
        let requests_for_task = Arc::clone(&requests);
        let handler = Arc::new(handler);

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = vec![0u8; 8192];
                let Ok(read) = socket.read(&mut buffer).await else {
                    continue;
                };
                if read == 0 {
                    continue;
                }
                let request = String::from_utf8_lossy(&buffer[..read]);
                let mut lines = request.split("\r\n");
                let request_line = lines.next().unwrap_or_default();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let mut headers = HashMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                requests_for_task
                    .lock()
                    .expect("request log")
                    .push(RequestRecord {
                        headers: headers.clone(),
                    });

                let response = handler(path, headers);
                let response_text = format!(
                    "HTTP/1.1 {} {}\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response.status,
                    reason_phrase(response.status),
                    response.content_type,
                    response.body.len(),
                    response.body,
                );
                let _ = socket.write_all(response_text.as_bytes()).await;
            }
        });

        MockServer {
            base_url: format!("http://{address}"),
            requests,
        }
    }

    fn reason_phrase(status: u16) -> &'static str {
        match status {
            200 => "OK",
            401 => "Unauthorized",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "Error",
        }
    }

    fn request(
        provider_name: &str,
        api: ApiKind,
        base_url: &str,
        api_key: Option<&str>,
    ) -> ModelDiscoveryRequest {
        ModelDiscoveryRequest {
            provider_name: provider_name.to_string(),
            api,
            base_url: base_url.to_string(),
            api_key: api_key.map(str::to_string),
            headers: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn openai_compatible_discovery_parses_models_json() {
        let server = spawn_mock_server(|path, _headers| {
            assert_eq!(path, "/v1/models");
            MockResponse::ok_json(
                r#"{"data":[{"id":"gpt-4o","name":"GPT-4o","context_length":128000,"modalities":["text","image"]},{"id":"o4-mini","capabilities":{"reasoning":true}}]}"#,
            )
        })
        .await;

        let models = discover_models(&request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        ))
        .await
        .expect("discover models");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].name, "GPT-4o");
        assert_eq!(models[0].context_length, Some(128_000));
        assert_eq!(models[0].supports_images, Some(true));
        assert_eq!(models[1].id, "o4-mini");
        assert_eq!(models[1].supports_reasoning, Some(true));
    }

    #[tokio::test]
    async fn anthropic_discovery_parses_models_json() {
        let server = spawn_mock_server(|path, headers| {
            assert_eq!(path, "/v1/models");
            assert_eq!(
                headers.get("anthropic-version"),
                Some(&"2023-06-01".to_string())
            );
            MockResponse::ok_json(
                r#"{"data":[{"id":"claude-sonnet-4-6","display_name":"Claude Sonnet 4.6","context_window":1000000,"capabilities":{"vision":true,"reasoning":true}}]}"#,
            )
        })
        .await;

        let models = discover_models(&request(
            "anthropic",
            ApiKind::AnthropicMessages,
            &server.base_url,
            Some("sk-ant"),
        ))
        .await
        .expect("discover anthropic models");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4-6");
        assert_eq!(models[0].name, "Claude Sonnet 4.6");
        assert_eq!(models[0].context_length, Some(1_000_000));
        assert_eq!(models[0].supports_images, Some(true));
        assert_eq!(models[0].supports_reasoning, Some(true));
    }

    #[tokio::test]
    async fn ollama_tags_parsing_normalizes_model_info() {
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3:32b","details":{"family":"qwen3","parameter_size":"32B","context_length":32768},"capabilities":["completion","vision"]}]}"#,
            ),
            // After PR 3 discovery always fans out /api/show.
            // Returning the authoritative capability list keeps
            // this test exercising the normalize-ModelInfo path
            // end-to-end, with thinking/vision populated from the
            // authoritative source rather than the substring
            // heuristic.
            "/api/show" => MockResponse::ok_json(
                r#"{"capabilities":["completion","thinking","vision"]}"#,
            ),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover ollama models");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3:32b");
        assert_eq!(models[0].name, "qwen3:32b (32B)");
        assert_eq!(models[0].context_length, Some(32_768));
        assert_eq!(models[0].supports_images, Some(true));
        assert_eq!(models[0].supports_reasoning, Some(true));
    }

    #[tokio::test]
    async fn auth_headers_are_attached_when_api_key_is_present() {
        let server = spawn_mock_server(|_path, _headers| {
            MockResponse::ok_json(r#"{"data":[{"id":"gpt-4o"}]}"#)
        })
        .await;

        let _ = discover_models(&request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        ))
        .await
        .expect("discover models");

        let requests = server.requests.lock().expect("request log");
        let auth = requests[0].headers.get("authorization").cloned();
        assert_eq!(auth.as_deref(), Some("Bearer sk-test"));
    }

    #[tokio::test]
    async fn auth_headers_are_omitted_when_api_key_is_absent() {
        let server = spawn_mock_server(|_path, _headers| {
            MockResponse::ok_json(r#"{"data":[{"id":"qwen3:32b"}]}"#)
        })
        .await;

        let _ = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover models");

        let requests = server.requests.lock().expect("request log");
        assert!(!requests[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn cache_hit_avoids_duplicate_network_calls() {
        let server = spawn_mock_server(|_path, _headers| {
            MockResponse::ok_json(r#"{"data":[{"id":"gpt-4o"}]}"#)
        })
        .await;
        let mut cache = ModelDiscoveryCache::new(Duration::from_secs(300));
        let request = request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        );

        let _ = cache.get_or_discover(&request).await.expect("first lookup");
        let _ = cache
            .get_or_discover(&request)
            .await
            .expect("second lookup");

        assert_eq!(server.requests.lock().expect("request log").len(), 1);
    }

    #[tokio::test]
    async fn cache_miss_triggers_network_discovery() {
        let server = spawn_mock_server(|_path, _headers| {
            MockResponse::ok_json(r#"{"data":[{"id":"gpt-4o"}]}"#)
        })
        .await;
        let mut cache = ModelDiscoveryCache::new(Duration::from_millis(1));
        let request = request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        );

        let _ = cache.get_or_discover(&request).await.expect("first lookup");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let _ = cache
            .get_or_discover(&request)
            .await
            .expect("second lookup");

        assert!(server.requests.lock().expect("request log").len() >= 2);
    }

    #[tokio::test]
    async fn explicit_refresh_bypasses_cache() {
        let server = spawn_mock_server(|_path, _headers| {
            MockResponse::ok_json(r#"{"data":[{"id":"gpt-4o"}]}"#)
        })
        .await;
        let mut cache = ModelDiscoveryCache::new(Duration::from_secs(300));
        let request = request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        );

        let _ = cache.get_or_discover(&request).await.expect("first lookup");
        let _ = cache.refresh(&request).await.expect("refresh lookup");

        assert_eq!(server.requests.lock().expect("request log").len(), 2);
    }

    #[tokio::test]
    async fn discovery_failure_is_not_cached() {
        let toggle = Arc::new(Mutex::new(true));
        let toggle_for_server = Arc::clone(&toggle);
        let server = spawn_mock_server(move |_path, _headers| {
            let mut should_fail = toggle_for_server.lock().expect("toggle");
            if *should_fail {
                *should_fail = false;
                MockResponse::status(401, "bad key")
            } else {
                MockResponse::ok_json(r#"{"data":[{"id":"gpt-4o"}]}"#)
            }
        })
        .await;
        let mut cache = ModelDiscoveryCache::new(Duration::from_secs(300));
        let request = request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        );

        let error = cache
            .get_or_discover(&request)
            .await
            .expect_err("first lookup should fail");
        assert!(matches!(error, ProviderError::Auth(message) if message == "bad key"));
        let _ = cache
            .get_or_discover(&request)
            .await
            .expect("second lookup");

        assert_eq!(server.requests.lock().expect("request log").len(), 2);
    }

    #[tokio::test]
    async fn unknown_json_fields_do_not_break_parsing() {
        let server = spawn_mock_server(|_path, _headers| {
            MockResponse::ok_json(
                r#"{"data":[{"id":"gpt-4o","name":"GPT-4o","extra":"ignored","capabilities":{"images":true,"extra":true}}],"extra_root":true}"#,
            )
        })
        .await;

        let models = discover_models(&request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        ))
        .await
        .expect("discover models");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].supports_images, Some(true));
    }

    /// Realistic slice of an OpenRouter `/api/v1/models` response.
    /// Covers: richer top-level fields (pricing, architecture,
    /// supported_parameters, top_provider), mixed reasoning /
    /// non-reasoning entries, and an entry that relies on
    /// `top_provider.context_length` because the top-level
    /// `context_length` is absent.
    const OPENROUTER_DISCOVERY_FIXTURE: &str = r#"{
        "data": [
            {
                "id": "anthropic/claude-sonnet-4",
                "name": "Anthropic: Claude Sonnet 4",
                "context_length": 200000,
                "pricing": {
                    "prompt": "0.000003",
                    "completion": "0.000015",
                    "request": "0",
                    "image": "0.0048"
                },
                "top_provider": {"context_length": 200000},
                "supported_parameters": ["tools", "tool_choice", "reasoning"],
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"]
                }
            },
            {
                "id": "openai/o3",
                "name": "OpenAI: o3",
                "pricing": {"prompt": "0.000002", "completion": "0.000008"},
                "top_provider": {
                    "context_length": 128000,
                    "max_completion_tokens": 65536
                },
                "supported_parameters": ["tools", "reasoning_effort"],
                "architecture": {
                    "input_modalities": ["text"],
                    "output_modalities": ["text"]
                }
            },
            {
                "id": "meta-llama/llama-3.1-8b-instruct",
                "name": "Llama 3.1 8B Instruct",
                "context_length": 131072,
                "pricing": {"prompt": "0.00000002", "completion": "0.00000005"},
                "supported_parameters": ["tools"],
                "architecture": {
                    "input_modalities": ["text"],
                    "output_modalities": ["text"]
                }
            }
        ]
    }"#;

    #[tokio::test]
    async fn openrouter_discovery_parses_full_response() {
        let server = spawn_mock_server(|path, _headers| {
            assert_eq!(path, "/v1/models");
            MockResponse::ok_json(OPENROUTER_DISCOVERY_FIXTURE)
        })
        .await;

        let models = discover_models(&request(
            "openrouter",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-or-test"),
        ))
        .await
        .expect("discover openrouter models");

        assert_eq!(models.len(), 3);

        // anthropic/claude-sonnet-4 — richest entry
        let claude = &models[0];
        assert_eq!(claude.id, "anthropic/claude-sonnet-4");
        assert_eq!(claude.name, "Anthropic: Claude Sonnet 4");
        assert_eq!(claude.provider, "openrouter");
        assert_eq!(claude.context_length, Some(200_000));
        assert_eq!(claude.supports_images, Some(true));
        assert_eq!(claude.supports_reasoning, Some(true));
        let pricing = claude.pricing.as_ref().expect("claude has pricing");
        assert_eq!(pricing.prompt.as_deref(), Some("0.000003"));
        assert_eq!(pricing.completion.as_deref(), Some("0.000015"));
        assert_eq!(pricing.request.as_deref(), Some("0"));
        assert_eq!(pricing.image.as_deref(), Some("0.0048"));
        let params = claude
            .supported_parameters
            .as_deref()
            .expect("supported_parameters preserved");
        assert!(params.iter().any(|p| p == "tools"));
        assert!(params.iter().any(|p| p == "reasoning"));

        // openai/o3 — relies on top_provider.context_length and
        // carries top_provider.max_completion_tokens so reasoning
        // runs don't get clipped by anie's 8 k default.
        let o3 = &models[1];
        assert_eq!(o3.id, "openai/o3");
        assert_eq!(
            o3.context_length,
            Some(128_000),
            "should fall back to top_provider.context_length"
        );
        assert_eq!(
            o3.max_output_tokens,
            Some(65_536),
            "should carry top_provider.max_completion_tokens"
        );
        assert_eq!(o3.supports_reasoning, Some(true));
        assert_eq!(o3.supports_images, Some(false));

        // meta-llama — no reasoning, text-only
        let llama = &models[2];
        assert_eq!(llama.id, "meta-llama/llama-3.1-8b-instruct");
        assert_eq!(llama.context_length, Some(131_072));
        assert_eq!(llama.supports_images, Some(false));
        assert_eq!(llama.supports_reasoning, Some(false));
        assert!(
            llama
                .supported_parameters
                .as_deref()
                .is_some_and(|params| params.iter().any(|p| p == "tools"))
        );
    }

    #[tokio::test]
    async fn openrouter_discovery_filters_non_tool_models() {
        // OpenRouter exposes many completion-only / image-gen
        // models that we can't drive from a coding agent. Only
        // entries whose `supported_parameters` includes `"tools"`
        // should survive discovery.
        let fixture = r#"{
            "data": [
                {
                    "id": "anthropic/claude-sonnet-4",
                    "supported_parameters": ["tools", "reasoning"]
                },
                {
                    "id": "some/completion-only-model",
                    "supported_parameters": ["temperature", "top_p"]
                },
                {
                    "id": "image-gen/flux-pro",
                    "supported_parameters": []
                },
                {
                    "id": "openai/o3",
                    "supported_parameters": ["tools", "reasoning_effort"]
                }
            ]
        }"#;

        let server = spawn_mock_server(move |path, _headers| {
            assert_eq!(path, "/v1/models");
            MockResponse::ok_json(fixture)
        })
        .await;

        let models = discover_models(&request(
            "openrouter",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-or-test"),
        ))
        .await
        .expect("discover openrouter models");

        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["anthropic/claude-sonnet-4", "openai/o3"]);
    }

    #[tokio::test]
    async fn copilot_discovery_keeps_chat_models_and_drops_others() {
        // Copilot's /models endpoint returns embedding, routing,
        // and completion-only entries alongside chat models.
        // Trying to use the non-chat ones produces
        // `HTTP 400 unsupported_api_for_model`. Discovery must
        // filter to models that advertise /chat/completions AND
        // type == "chat" AND model_picker_enabled != false.
        let fixture = r#"{
            "data": [
                {
                    "id": "claude-sonnet-4.6",
                    "type": "chat",
                    "supported_endpoints": ["/v1/messages", "/chat/completions"],
                    "model_picker_enabled": true
                },
                {
                    "id": "text-embedding-3-small",
                    "type": "embeddings",
                    "supported_endpoints": ["/embeddings"]
                },
                {
                    "id": "accounts/msft/routers/abc",
                    "type": "chat",
                    "supported_endpoints": ["/chat/completions"],
                    "model_picker_enabled": false
                },
                {
                    "id": "gpt-5.4-mini",
                    "type": "completion",
                    "supported_endpoints": ["/completions"]
                },
                {
                    "id": "gpt-4.1",
                    "type": "chat",
                    "supported_endpoints": ["/chat/completions"],
                    "model_picker_enabled": true
                }
            ]
        }"#;

        let server = spawn_mock_server(move |path, _headers| {
            // Copilot discovery hits `/models` (no /v1 prefix).
            assert_eq!(path, "/models");
            MockResponse::ok_json(fixture)
        })
        .await;

        let models = discover_models(&request(
            "github-copilot",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("copilot-token"),
        ))
        .await
        .expect("discover copilot models");

        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["claude-sonnet-4.6", "gpt-4.1"]);
    }

    #[tokio::test]
    async fn copilot_context_window_parsed_from_nested_capability_limits() {
        // Regression: Copilot's /models nests context under
        // capabilities.limits.max_context_window_tokens, not at
        // the top level like OpenAI / OpenRouter do. Missing
        // this caused every Copilot model to default to 32k in
        // the status bar even though Claude Opus serves 200k
        // through Copilot.
        let fixture = r#"{
            "data": [{
                "id": "claude-opus-4.7",
                "type": "chat",
                "supported_endpoints": ["/chat/completions"],
                "model_picker_enabled": true,
                "capabilities": {
                    "family": "claude-opus-4.7",
                    "limits": {
                        "max_context_window_tokens": 200000,
                        "max_output_tokens": 64000,
                        "max_prompt_tokens": 128000
                    }
                }
            }]
        }"#;

        let server = spawn_mock_server(move |_path, _headers| MockResponse::ok_json(fixture)).await;

        let models = discover_models(&request(
            "github-copilot",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("copilot-token"),
        ))
        .await
        .expect("discover");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].context_length, Some(200_000));
        assert_eq!(models[0].max_output_tokens, Some(64_000));
    }

    #[tokio::test]
    async fn copilot_filter_does_not_drop_entries_missing_optional_fields() {
        // Defensive: an entry with neither `type` nor
        // `supported_endpoints` nor `model_picker_enabled` was
        // valid before the filter and should stay valid for
        // non-Copilot providers. (We never hit this path for
        // Copilot itself since its responses always carry these
        // fields, but the absence defaults are what keep other
        // OpenAI-compat providers unaffected.)
        let fixture = r#"{
            "data": [{"id": "gpt-4o"}]
        }"#;

        let server = spawn_mock_server(move |_path, _headers| MockResponse::ok_json(fixture)).await;

        let models = discover_models(&request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        ))
        .await
        .expect("discover");
        assert_eq!(models.len(), 1);
    }

    #[tokio::test]
    async fn tool_supporting_filter_does_not_apply_to_non_openrouter_providers() {
        // Direct OpenAI discovery doesn't return
        // `supported_parameters` at all; applying the filter
        // would wipe out the catalog. Guard against that.
        let fixture = r#"{
            "data": [
                {"id": "gpt-4o"},
                {"id": "o4-mini"}
            ]
        }"#;

        let server = spawn_mock_server(move |_path, _headers| MockResponse::ok_json(fixture)).await;

        let models = discover_models(&request(
            "openai",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("sk-test"),
        ))
        .await
        .expect("discover openai models");

        assert_eq!(models.len(), 2);
    }

    #[test]
    fn qwen3_5_is_not_classified_as_reasoning_family() {
        // Regression: see docs/ollama_capability_discovery/README.md
        // PR 1. Both the raw-id and Ollama-family-metadata call
        // sites must reject `qwen3.5`-style inputs.
        assert!(!reasoning_family("qwen3.5:9b"));
        assert!(!reasoning_family("qwen3.5"));
        assert!(!reasoning_family("qwen35"));
    }

    #[test]
    fn qwen3_32b_remains_classified_as_reasoning_family() {
        assert!(reasoning_family("qwen3:32b"));
        assert!(reasoning_family("qwen3-coder"));
        assert!(reasoning_family("qwen3"));
    }

    #[test]
    fn reasoning_family_accepts_known_families_rejects_unknowns() {
        assert!(reasoning_family("qwq:32b"));
        assert!(reasoning_family("qwq"));
        assert!(reasoning_family("deepseek-r1:7b"));
        assert!(reasoning_family("deepseek-r1-distill-qwen-7b"));
        assert!(reasoning_family("deepseek-r1"));
        assert!(reasoning_family("gpt-oss:20b"));
        assert!(reasoning_family("gpt-oss-large"));
        assert!(reasoning_family("gpt-oss"));
        // Unknowns stay unclassified.
        assert!(!reasoning_family("llama3.1:8b"));
        assert!(!reasoning_family("gemma3:1b"));
        assert!(!reasoning_family("mistral:7b"));
        // Conservative on hypothetical future variants.
        assert!(!reasoning_family("deepseek-r1.5:14b"));
        assert!(!reasoning_family("gpt-oss.1:7b"));
    }

    #[test]
    fn infer_reasoning_fallback_respects_family_boundaries() {
        // Capability-less path: the heuristic fallback in
        // `infer_reasoning` must use the same tightened
        // family-prefix rule. No `capabilities`, no
        // `supported_parameters` — the lowest-priority fallback.
        assert_eq!(
            infer_reasoning("ollama", "qwen3.5:9b", None, None),
            Some(false),
        );
        assert_eq!(
            infer_reasoning("ollama", "qwen3:32b", None, None),
            Some(true),
        );
        // `model_id.contains("reason")` and `starts_with('o')`
        // still apply — they're independent of the family rule.
        assert_eq!(
            infer_reasoning("openai", "o4-mini", None, None),
            Some(true),
        );
        // Anthropic provider short-circuit still applies.
        assert_eq!(
            infer_reasoning("anthropic", "claude-sonnet-4-6", None, None),
            Some(true),
        );
    }

    #[tokio::test]
    async fn ollama_discovery_uses_show_capabilities_when_available() {
        // Authoritative path: /api/tags lists the model, /api/show
        // returns its true capabilities. Must populate
        // supports_reasoning from `thinking` token,
        // supports_images from `vision`, provider_capabilities
        // with the full list.
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3:32b","model":"qwen3:32b","details":{"family":"qwen3"}}]}"#,
            ),
            "/api/show" => MockResponse::ok_json(
                r#"{"capabilities":["completion","tools","thinking","vision"]}"#,
            ),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover ollama models");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3:32b");
        assert_eq!(models[0].supports_reasoning, Some(true));
        assert_eq!(models[0].supports_images, Some(true));
        assert_eq!(
            models[0].provider_capabilities.as_deref(),
            Some(
                &[
                    "completion".to_string(),
                    "tools".into(),
                    "thinking".into(),
                    "vision".into()
                ][..]
            )
        );
    }

    #[tokio::test]
    async fn qwen3_5_via_show_capabilities_is_thinking_capable() {
        // The real-world case: /api/show for `qwen3.5:9b` returns
        // `"thinking"` in its capabilities even though PR 1's
        // tightened heuristic would reject it. Authoritative data
        // wins: supports_reasoning = Some(true).
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3.5:9b","model":"qwen3.5:9b","details":{"family":"qwen3"}}]}"#,
            ),
            "/api/show" => {
                MockResponse::ok_json(r#"{"capabilities":["completion","thinking"]}"#)
            }
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover ollama models");

        assert_eq!(models[0].supports_reasoning, Some(true));
    }

    #[tokio::test]
    async fn ollama_discovery_falls_back_to_heuristic_when_show_fails() {
        // /api/tags succeeds, /api/show 500s. The whole discovery
        // call must still succeed, with that specific model
        // falling back to the substring heuristic.
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[
                    {"name":"qwen3:32b","model":"qwen3:32b","details":{"family":"qwen3"}},
                    {"name":"gemma3:1b","model":"gemma3:1b","details":{"family":"gemma3"}}
                ]}"#,
            ),
            "/api/show" => MockResponse::status(500, "boom"),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover must succeed even when show fails");

        assert_eq!(models.len(), 2);
        // Heuristic fallback: qwen3:32b passes family-prefix,
        // gemma3:1b doesn't.
        let qwen = models.iter().find(|m| m.id == "qwen3:32b").expect("qwen");
        let gemma = models.iter().find(|m| m.id == "gemma3:1b").expect("gemma");
        assert_eq!(qwen.supports_reasoning, Some(true));
        assert_eq!(gemma.supports_reasoning, Some(false));
        assert!(qwen.provider_capabilities.is_none());
        assert!(gemma.provider_capabilities.is_none());
    }

    #[tokio::test]
    async fn ollama_show_failure_does_not_fail_overall_discovery() {
        // Twin of the preceding test named to match the plan's
        // exit criteria. 500 on /api/show must not propagate as
        // an outer Err.
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3:32b","model":"qwen3:32b"}]}"#,
            ),
            "/api/show" => MockResponse::status(500, "server error"),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let result = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await;

        assert!(
            result.is_ok(),
            "discovery must tolerate /api/show failures, got {result:?}"
        );
    }

    #[tokio::test]
    async fn unknown_capability_token_is_preserved_in_provider_capabilities() {
        // Forward-compat: a capability token anie doesn't know
        // today must still round-trip through
        // provider_capabilities so the catalog can represent it
        // without a schema bump.
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"future:7b","model":"future:7b"}]}"#,
            ),
            "/api/show" => MockResponse::ok_json(
                r#"{"capabilities":["completion","future-capability-xyz"]}"#,
            ),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover");
        assert_eq!(
            models[0].provider_capabilities.as_deref(),
            Some(&["completion".to_string(), "future-capability-xyz".into()][..])
        );
    }

    #[tokio::test]
    async fn ollama_show_extracts_context_length_using_architecture_prefix() {
        // /api/show.model_info carries context length keyed by
        // `{general.architecture}.context_length`.
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3.5:9b","model":"qwen3.5:9b"}]}"#,
            ),
            "/api/show" => MockResponse::ok_json(
                r#"{"capabilities":["completion"],"model_info":{"general.architecture":"qwen35","qwen35.context_length":262144}}"#,
            ),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover");
        assert_eq!(models[0].context_length, Some(262_144));
    }

    #[tokio::test]
    async fn ollama_show_handles_missing_context_length_field() {
        // No architecture key, no arch.context_length → the
        // discovery layer must not panic and context_length
        // falls back to whatever /api/tags provided (None here).
        let server = spawn_mock_server(|path, _headers| match path.as_str() {
            "/api/tags" => MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3.5:9b","model":"qwen3.5:9b"}]}"#,
            ),
            "/api/show" => MockResponse::ok_json(r#"{"capabilities":["completion"]}"#),
            _ => MockResponse::status(404, "not found"),
        })
        .await;

        let models = discover_models(&request(
            "ollama",
            ApiKind::OpenAICompletions,
            &server.base_url,
            None,
        ))
        .await
        .expect("discover");
        assert_eq!(models[0].context_length, None);
    }

    #[tokio::test]
    async fn openrouter_discovery_falls_back_when_fetch_fails() {
        let server =
            spawn_mock_server(|_path, _headers| MockResponse::status(401, "invalid api key")).await;

        let error = discover_models(&request(
            "openrouter",
            ApiKind::OpenAICompletions,
            &server.base_url,
            Some("bad-key"),
        ))
        .await
        .expect_err("bad key should fail");

        // Caller (onboarding) preserves the credential and surfaces
        // this as a recoverable message; the discovery layer just
        // needs to report the typed error cleanly.
        assert!(
            matches!(error, ProviderError::Auth(_)),
            "expected typed auth error, got {error:?}"
        );
    }
}
