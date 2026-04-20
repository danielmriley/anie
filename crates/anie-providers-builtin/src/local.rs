use std::time::Duration;

use anie_provider::{
    ApiKind, CostPerMillion, Model, ModelCompat, ReasoningCapabilities, ReasoningControlMode,
    ReasoningOutputMode, ThinkingRequestMode,
};

/// A detected local OpenAI-compatible server.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalServer {
    /// Server name, e.g. `ollama` or `lmstudio`.
    pub name: String,
    /// Base server URL without the `/v1` suffix.
    pub base_url: String,
    /// Models reported by the server.
    pub models: Vec<Model>,
}

fn native_separated_reasoning_capabilities(
    request_mode: ThinkingRequestMode,
) -> ReasoningCapabilities {
    ReasoningCapabilities {
        control: Some(ReasoningControlMode::Native),
        output: Some(ReasoningOutputMode::Separated),
        tags: None,
        request_mode: Some(request_mode),
    }
}

fn prompt_only_reasoning_capabilities() -> ReasoningCapabilities {
    ReasoningCapabilities {
        control: Some(ReasoningControlMode::Prompt),
        output: None,
        tags: None,
        request_mode: Some(ThinkingRequestMode::PromptSteering),
    }
}

fn is_local_host(provider: &str, base_url: &str) -> bool {
    let provider = provider.to_ascii_lowercase();
    let base_url = base_url.to_ascii_lowercase();

    matches!(provider.as_str(), "ollama" | "lmstudio" | "vllm")
        || base_url.starts_with("http://localhost")
        || base_url.starts_with("https://localhost")
        || base_url.starts_with("http://127.0.0.1")
        || base_url.starts_with("https://127.0.0.1")
        || base_url.starts_with("http://[::1]")
        || base_url.starts_with("https://[::1]")
}

fn is_reasoning_capable_family(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    ["qwen3", "qwq", "deepseek-r1", "gpt-oss"]
        .iter()
        .any(|family| model_id.contains(family))
}

/// Conservative default reasoning profile for local OpenAI-compatible models.
#[must_use]
pub fn default_local_reasoning_capabilities(
    provider: &str,
    base_url: &str,
    model_id: &str,
) -> Option<ReasoningCapabilities> {
    if !is_local_host(provider, base_url) {
        return None;
    }

    let provider = provider.to_ascii_lowercase();
    let known_native_backend = provider == "ollama"
        || provider == "lmstudio"
        || provider == "vllm"
        || base_url.contains(":11434")
        || base_url.contains(":1234");

    if known_native_backend && is_reasoning_capable_family(model_id) {
        let request_mode = if provider == "lmstudio" || base_url.contains(":1234") {
            ThinkingRequestMode::NestedReasoning
        } else {
            ThinkingRequestMode::ReasoningEffort
        };
        Some(native_separated_reasoning_capabilities(request_mode))
    } else {
        Some(prompt_only_reasoning_capabilities())
    }
}

/// Detect commonly-used local model servers using the OpenAI-compatible `/v1/models` route.
///
/// Returns an empty vec if the detection HTTP client cannot be built
/// (TLS roots unavailable, etc.) — discovery is a best-effort
/// feature and should never prevent startup. A warning is logged so
/// the failure is visible.
pub async fn detect_local_servers() -> Vec<LocalServer> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(
                %error,
                "failed to build local-detection HTTP client; skipping local-server discovery"
            );
            return Vec::new();
        }
    };

    let mut servers = Vec::new();
    if let Some(server) = probe_openai_compatible(&client, "ollama", "http://localhost:11434").await
    {
        servers.push(server);
    }
    if let Some(server) =
        probe_openai_compatible(&client, "lmstudio", "http://localhost:1234").await
    {
        servers.push(server);
    }
    servers
}

/// Probe a single OpenAI-compatible base URL for `/v1/models` support.
pub async fn probe_openai_compatible(
    client: &reqwest::Client,
    name: &str,
    base_url: &str,
) -> Option<LocalServer> {
    let response = client
        .get(format!("{}/v1/models", base_url.trim_end_matches('/')))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }

    let body: serde_json::Value = response.json().await.ok()?;
    let models = body
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            Some(Model {
                id: id.to_string(),
                name: id.to_string(),
                provider: name.to_string(),
                api: ApiKind::OpenAICompletions,
                base_url: format!("{}/v1", base_url.trim_end_matches('/')),
                context_window: 32_768,
                max_tokens: 8_192,
                supports_reasoning: false,
                reasoning_capabilities: default_local_reasoning_capabilities(name, base_url, id),
                supports_images: false,
                cost_per_million: CostPerMillion::zero(),
                replay_capabilities: None,
                compat: ModelCompat::None,
            })
        })
        .collect::<Vec<_>>();
    if models.is_empty() {
        return None;
    }

    Some(LocalServer {
        name: name.to_string(),
        base_url: base_url.trim_end_matches('/').to_string(),
        models,
    })
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn probe_detects_openai_compatible_server() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request_buffer = [0u8; 1024];
            let _ = socket.read(&mut request_buffer).await;
            let body = r#"{"data":[{"id":"qwen3:32b"}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("client");
        let detected = probe_openai_compatible(&client, "ollama", &format!("http://{address}"))
            .await
            .expect("detected local server");

        assert_eq!(detected.name, "ollama");
        assert_eq!(detected.base_url, format!("http://{address}"));
        assert_eq!(detected.models.len(), 1);
        assert_eq!(detected.models[0].id, "qwen3:32b");
        assert_eq!(detected.models[0].provider, "ollama");
        assert_eq!(detected.models[0].base_url, format!("http://{address}/v1"));
        assert!(!detected.models[0].supports_reasoning);
        assert_eq!(
            detected.models[0].reasoning_capabilities,
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            })
        );

        server.await.expect("server task");
    }

    #[test]
    fn default_local_reasoning_capabilities_are_conservative_and_explainable() {
        assert_eq!(
            default_local_reasoning_capabilities(
                "ollama",
                "http://localhost:11434/v1",
                "qwen3:32b"
            ),
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            })
        );
        assert_eq!(
            default_local_reasoning_capabilities(
                "lmstudio",
                "http://localhost:1234/v1",
                "qwen3:32b"
            ),
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::NestedReasoning),
            })
        );
        assert_eq!(
            default_local_reasoning_capabilities(
                "custom",
                "http://localhost:8080/v1",
                "unknown-model"
            ),
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Prompt),
                output: None,
                tags: None,
                request_mode: Some(ThinkingRequestMode::PromptSteering),
            })
        );
        assert_eq!(
            default_local_reasoning_capabilities("openai", "https://api.openai.com/v1", "o4-mini"),
            None
        );
    }

    #[tokio::test]
    async fn probe_times_out_quickly_when_server_is_missing() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(150))
            .build()
            .expect("client");
        let started = std::time::Instant::now();
        let detected = probe_openai_compatible(&client, "missing", "http://127.0.0.1:9").await;
        assert!(detected.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
