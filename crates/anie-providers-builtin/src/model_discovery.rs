use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};

use anie_provider::{ApiKind, ModelInfo, ProviderError};
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
    }
}

/// In-memory TTL cache for provider model discovery.
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
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = request.cache_key();
        if let Some(entry) = self.entries.get(&key)
            && entry.fetched_at.elapsed() < self.default_ttl
        {
            return Ok(entry.models.clone());
        }

        let models = discover_models(request).await?;
        self.entries.insert(
            key,
            CacheEntry {
                models: models.clone(),
                fetched_at: Instant::now(),
            },
        );
        Ok(models)
    }

    /// Force a fresh discovery and replace any cached entry.
    pub async fn refresh(
        &mut self,
        request: &ModelDiscoveryRequest,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let key = request.cache_key();
        let models = discover_models(request).await?;
        self.entries.insert(
            key,
            CacheEntry {
                models: models.clone(),
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
    models: Vec<ModelInfo>,
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
    let url = format!("{}/models", normalize_openai_base_url(&request.base_url));
    let response = send_request(&client, request, url, AuthStyle::Bearer).await?;
    let body = response
        .json::<OpenAiModelsResponse>()
        .await
        .map_err(|error| {
            ProviderError::InvalidStreamJson(format!(
                "failed to parse OpenAI-compatible model list: {error}"
            ))
        })?;

    Ok(body
        .data
        .into_iter()
        .filter(|entry| entry.object.as_deref().unwrap_or("model") == "model")
        .map(|entry| ModelInfo {
            id: entry.id.clone(),
            name: entry.name.unwrap_or(entry.id.clone()),
            provider: request.provider_name.clone(),
            context_length: entry
                .context_length
                .or(entry.context_window)
                .or(entry.max_context_tokens)
                .or(entry.input_token_limit),
            supports_images: infer_openai_images(
                entry.modalities.as_deref(),
                entry.capabilities.as_ref(),
            ),
            supports_reasoning: infer_reasoning(
                request.provider_name.as_str(),
                &entry.id,
                entry.capabilities.as_ref(),
            ),
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
        })
        .collect())
}

async fn discover_ollama_tags(
    request: &ModelDiscoveryRequest,
) -> Result<Vec<ModelInfo>, ProviderError> {
    let client = discovery_http_client()?;
    let url = format!("{}/api/tags", normalize_root_base_url(&request.base_url));
    let response = send_request(&client, request, url, AuthStyle::Bearer).await?;
    let body = response
        .json::<OllamaTagsResponse>()
        .await
        .map_err(|error| {
            ProviderError::InvalidStreamJson(format!("failed to parse Ollama tag list: {error}"))
        })?;

    Ok(body
        .models
        .into_iter()
        .filter_map(|entry| {
            let id = entry.model.or(entry.name)?;
            let supports_reasoning = Some(
                entry
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
                    || reasoning_family(&id),
            );
            let supports_images = Some(
                entry
                    .capabilities
                    .as_ref()
                    .is_some_and(|caps| caps.iter().any(|cap| cap.eq_ignore_ascii_case("vision"))),
            );
            let context_length = entry
                .details
                .as_ref()
                .and_then(|details| details.context_length.or(details.context_window));
            let name = ollama_display_name(&id, entry.details.as_ref());

            Some(ModelInfo {
                id,
                name,
                provider: request.provider_name.clone(),
                context_length,
                supports_images,
                supports_reasoning,
            })
        })
        .collect())
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
) -> Option<bool> {
    capabilities
        .and_then(|caps| caps.vision.or(caps.images))
        .or_else(|| {
            modalities.map(|modalities| {
                modalities.iter().any(|modality| {
                    modality.eq_ignore_ascii_case("image")
                        || modality.eq_ignore_ascii_case("vision")
                })
            })
        })
}

fn infer_reasoning(
    provider_name: &str,
    model_id: &str,
    capabilities: Option<&ModelCapabilities>,
) -> Option<bool> {
    capabilities.and_then(|caps| caps.reasoning).or_else(|| {
        let provider_name = provider_name.to_ascii_lowercase();
        let model_id = model_id.to_ascii_lowercase();
        let reasoning = model_id.contains("reason")
            || model_id.starts_with('o')
            || model_id.contains("qwen3")
            || model_id.contains("qwq")
            || model_id.contains("deepseek-r1")
            || model_id.contains("gpt-oss")
            || provider_name == "anthropic";
        Some(reasoning)
    })
}

fn reasoning_family(family: &str) -> bool {
    let family = family.to_ascii_lowercase();
    family.contains("qwen3")
        || family.contains("qwq")
        || family.contains("deepseek-r1")
        || family.contains("gpt-oss")
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
    items.sort_by_key(|(key, _)| *key);
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
        let server = spawn_mock_server(|path, _headers| {
            assert_eq!(path, "/api/tags");
            MockResponse::ok_json(
                r#"{"models":[{"name":"qwen3:32b","details":{"family":"qwen3","parameter_size":"32B","context_length":32768},"capabilities":["completion","vision"]}]}"#,
            )
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
}
