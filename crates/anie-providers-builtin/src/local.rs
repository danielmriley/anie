use std::time::Duration;

use anie_provider::{
    ApiKind, CostPerMillion, Model, ModelCompat, ReasoningCapabilities, ReasoningControlMode,
    ReasoningOutputMode, ThinkingRequestMode,
};

use crate::model_discovery::fetch_ollama_show_capabilities;

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

/// Pre-normalized provider + base-URL inputs for the local-
/// reasoning helpers. Hoisting these out of a per-model loop
/// avoids re-lowercasing the same strings once per model.
/// Plan 06 PR-F.
pub struct LocalProbeInputs {
    provider_lower: String,
    base_url_lower: String,
}

impl LocalProbeInputs {
    /// Compute lowercase copies of the invariant probe inputs.
    /// Call once per discovery response, then pass by reference
    /// into [`default_local_reasoning_capabilities_normalized`]
    /// for each model entry.
    #[must_use]
    pub fn new(provider: &str, base_url: &str) -> Self {
        Self {
            provider_lower: provider.to_ascii_lowercase(),
            base_url_lower: base_url.to_ascii_lowercase(),
        }
    }
}

fn is_local_host_normalized(provider_lower: &str, base_url_lower: &str) -> bool {
    matches!(provider_lower, "ollama" | "lmstudio" | "vllm")
        || base_url_lower.starts_with("http://localhost")
        || base_url_lower.starts_with("https://localhost")
        || base_url_lower.starts_with("http://127.0.0.1")
        || base_url_lower.starts_with("https://127.0.0.1")
        || base_url_lower.starts_with("http://[::1]")
        || base_url_lower.starts_with("https://[::1]")
}

/// Model-id families known to support leveled thinking (Ollama's
/// native `think: "low"|"medium"|"high"`). Kept in sync with the
/// lists in `model_discovery::reasoning_family` and
/// `model_discovery::infer_reasoning`.
const REASONING_FAMILIES: &[&str] = &["qwen3", "qwq", "deepseek-r1", "gpt-oss"];

/// True when `id` equals `family`, or starts with `family:` or
/// `family-` — i.e., `family` is the leading segment of the id
/// delimited by a tag/variant separator.
///
/// Substring matching on `contains(family)` used to mis-classify
/// `qwen3.5:9b` (which does not support leveled thinking) as the
/// `qwen3` family, causing Ollama to 400 the request with
/// `think value "low" is not supported for this model`. See
/// `docs/ollama_capability_discovery/README.md` PR 1.
fn id_matches_reasoning_family(id: &str, family: &str) -> bool {
    if id == family {
        return true;
    }
    match id.strip_prefix(family) {
        Some(rest) => matches!(rest.chars().next(), Some(':' | '-')),
        None => false,
    }
}

fn is_reasoning_capable_family(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    REASONING_FAMILIES
        .iter()
        .any(|family| id_matches_reasoning_family(&id, family))
}

/// Plan 06 PR-F: normalized variant of
/// [`default_local_reasoning_capabilities`]. Accepts pre-
/// lowercased provider + base URL so the per-model loop in
/// local server discovery doesn't lowercase the same
/// invariant inputs N times.
#[must_use]
pub fn default_local_reasoning_capabilities_normalized(
    inputs: &LocalProbeInputs,
    model_id: &str,
) -> Option<ReasoningCapabilities> {
    if !is_local_host_normalized(&inputs.provider_lower, &inputs.base_url_lower) {
        return None;
    }
    let provider = inputs.provider_lower.as_str();
    let base_url = inputs.base_url_lower.as_str();
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

/// Conservative default reasoning profile for local OpenAI-compatible models.
/// Convenience wrapper around
/// [`default_local_reasoning_capabilities_normalized`] that
/// computes `LocalProbeInputs` per call — prefer the
/// normalized variant in loops.
#[must_use]
pub fn default_local_reasoning_capabilities(
    provider: &str,
    base_url: &str,
    model_id: &str,
) -> Option<ReasoningCapabilities> {
    let inputs = LocalProbeInputs::new(provider, base_url);
    default_local_reasoning_capabilities_normalized(&inputs, model_id)
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
    // Plan 06 PR-F: hoist the invariants out of the per-model
    // filter_map. The trimmed + /v1-suffixed URLs, and the
    // lowercased probe inputs, are all identical for every
    // model in a single /v1/models response.
    let trimmed_base_url = base_url.trim_end_matches('/').to_string();
    let v1_base_url = format!("{trimmed_base_url}/v1");
    let is_ollama = is_ollama_probe_target(name, &trimmed_base_url);
    let probe_inputs = LocalProbeInputs::new(name, base_url);
    let mut models = body
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|model| {
            let id = model.get("id")?.as_str()?;
            Some(Model {
                id: id.to_string(),
                name: id.to_string(),
                provider: name.to_string(),
                api: if is_ollama {
                    ApiKind::OllamaChatApi
                } else {
                    ApiKind::OpenAICompletions
                },
                base_url: if is_ollama {
                    trimmed_base_url.clone()
                } else {
                    v1_base_url.clone()
                },
                context_window: 32_768,
                max_tokens: 8_192,
                supports_reasoning: false,
                reasoning_capabilities: default_local_reasoning_capabilities_normalized(
                    &probe_inputs,
                    id,
                ),
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

    // When the probed server is Ollama, upgrade heuristic
    // capabilities with authoritative `/api/show` data — same
    // pipeline as `discover_ollama_tags` in PR 3, applied here so
    // the local-server-detection path during onboarding /
    // bootstrap gets the authoritative signal too. Per-model
    // failure logs and keeps the heuristic for that model.
    // See `docs/ollama_capability_discovery/README.md` PR 5.
    if is_ollama {
        let show_futures = models
            .iter()
            .map(|model| fetch_ollama_show_capabilities(client, &trimmed_base_url, &model.id));
        let show_results = futures::future::join_all(show_futures).await;
        for (model, result) in models.iter_mut().zip(show_results) {
            match result {
                Ok(Some(show)) => {
                    let caps_lower: Vec<String> = show
                        .capabilities
                        .as_ref()
                        .map(|caps| caps.iter().map(|cap| cap.to_ascii_lowercase()).collect())
                        .unwrap_or_default();
                    let has_thinking = caps_lower.iter().any(|cap| cap == "thinking");
                    let has_vision = caps_lower.iter().any(|cap| cap == "vision");
                    model.supports_reasoning = has_thinking;
                    model.supports_images = has_vision;
                    // Authoritative: thinking-capable → native
                    // ReasoningEffort profile; non-thinking →
                    // clear any heuristic-inferred profile so the
                    // user's thinking level is silently dropped
                    // (see invariant tests in PR 5).
                    model.reasoning_capabilities = if has_thinking {
                        Some(ReasoningCapabilities {
                            control: Some(ReasoningControlMode::Native),
                            output: Some(ReasoningOutputMode::Separated),
                            tags: None,
                            request_mode: Some(ThinkingRequestMode::ReasoningEffort),
                        })
                    } else {
                        None
                    };
                }
                Ok(None) => {
                    // Keep heuristic defaults from the filter_map
                    // above.
                }
                Err(error) => {
                    tracing::warn!(
                        model = %model.id,
                        %error,
                        "Ollama /api/show probe failed during local detection; keeping heuristic"
                    );
                }
            }
        }
    }

    Some(LocalServer {
        name: name.to_string(),
        base_url: trimmed_base_url,
        models,
    })
}

/// True when the `(name, base_url)` pair identifies an Ollama
/// server that should be probed via `/api/show`. Matches the
/// detection used in `model_discovery::should_try_ollama_tags`.
fn is_ollama_probe_target(name: &str, base_url: &str) -> bool {
    name.eq_ignore_ascii_case("ollama") || base_url.contains(":11434")
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
        assert_eq!(detected.models[0].api, ApiKind::OllamaChatApi);
        assert_eq!(detected.models[0].base_url, format!("http://{address}"));
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

    #[tokio::test]
    async fn non_ollama_local_probes_still_use_openai_completions() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request_buffer = [0u8; 1024];
            let _ = socket.read(&mut request_buffer).await;
            let body = r#"{"data":[{"id":"local-model"}]}"#;
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
        let detected = probe_openai_compatible(&client, "lmstudio", &format!("http://{address}"))
            .await
            .expect("detected local server");

        assert_eq!(detected.models[0].api, ApiKind::OpenAICompletions);
        assert_eq!(detected.models[0].base_url, format!("http://{address}/v1"));

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

    #[test]
    fn qwen3_5_is_not_classified_as_reasoning_capable_family() {
        // Regression: `"qwen3.5:9b".contains("qwen3")` used to
        // mis-classify qwen3.5 as a leveled-thinking model,
        // producing HTTP 400 `think value "low" is not supported
        // for this model` from Ollama. See
        // docs/ollama_capability_discovery/README.md PR 1.
        assert!(!is_reasoning_capable_family("qwen3.5:9b"));
        assert!(!is_reasoning_capable_family("qwen3.5"));
        assert!(!is_reasoning_capable_family("qwen35:7b"));
        // local-reasoning-capabilities should now return the
        // conservative prompt-steering profile, matching the
        // fall-through for unknown local models.
        assert_eq!(
            default_local_reasoning_capabilities(
                "ollama",
                "http://localhost:11434/v1",
                "qwen3.5:9b"
            ),
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Prompt),
                output: None,
                tags: None,
                request_mode: Some(ThinkingRequestMode::PromptSteering),
            })
        );
    }

    #[test]
    fn qwen3_32b_remains_classified_as_reasoning_capable_family() {
        // Guardrail: the fix above must not regress the genuine
        // qwen3 variants.
        assert!(is_reasoning_capable_family("qwen3:32b"));
        assert!(is_reasoning_capable_family("qwen3:8b"));
        assert!(is_reasoning_capable_family("qwen3-coder"));
        assert!(is_reasoning_capable_family("qwen3"));
    }

    #[test]
    fn gpt_oss_remains_classified() {
        assert!(is_reasoning_capable_family("gpt-oss:20b"));
        assert!(is_reasoning_capable_family("gpt-oss-large"));
        assert!(is_reasoning_capable_family("gpt-oss"));
        // Not a gpt-oss model.
        assert!(!is_reasoning_capable_family("gpt-oss.1:7b"));
    }

    #[test]
    fn qwq_remains_classified() {
        assert!(is_reasoning_capable_family("qwq:32b"));
        assert!(is_reasoning_capable_family("qwq-preview"));
        assert!(is_reasoning_capable_family("qwq"));
    }

    #[test]
    fn deepseek_r1_remains_classified() {
        assert!(is_reasoning_capable_family("deepseek-r1:7b"));
        assert!(is_reasoning_capable_family("deepseek-r1-distill-qwen-7b"));
        assert!(is_reasoning_capable_family("deepseek-r1"));
        // Conservative: deepseek-r1.5 is a hypothetical future
        // variant that should NOT inherit the leveled-thinking
        // assumption without explicit config.
        assert!(!is_reasoning_capable_family("deepseek-r1.5:14b"));
    }

    #[tokio::test]
    async fn local_probe_attaches_show_capabilities_for_ollama() {
        // PR 5 mirror of PR 3: when the local probe detects
        // Ollama (name == "ollama"), it follows up with
        // `/api/show` per model and upgrades the heuristic
        // capabilities to the authoritative Native+ReasoningEffort
        // profile (thinking) or clears them entirely (non-thinking).
        use std::sync::{Arc, Mutex};

        // Per-connection handler: first connection is
        // `/v1/models`, all subsequent are `/api/show`.
        let call_count = Arc::new(Mutex::new(0u32));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local server");
        let address = listener.local_addr().expect("local addr");
        let counter = Arc::clone(&call_count);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut request_buffer = vec![0u8; 8192];
                let Ok(_read) = socket.read(&mut request_buffer).await else {
                    continue;
                };
                let request = String::from_utf8_lossy(&request_buffer);
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let body = match path.as_str() {
                    "/v1/models" => {
                        let mut n = counter.lock().expect("counter");
                        *n += 1;
                        r#"{"data":[{"id":"qwen3:32b"},{"id":"gemma3:1b"}]}"#.to_string()
                    }
                    "/api/show" => {
                        let mut n = counter.lock().expect("counter");
                        *n += 1;
                        // Return thinking for qwen3:32b and not
                        // for gemma3:1b. We can't peek at the
                        // POSTed body reliably through this
                        // minimal mock, so we alternate: first
                        // /api/show call → thinking, second →
                        // no thinking. Both models will probe
                        // in the order returned by /v1/models.
                        if *n == 2 {
                            r#"{"capabilities":["completion","thinking"]}"#.to_string()
                        } else {
                            r#"{"capabilities":["completion"]}"#.to_string()
                        }
                    }
                    _ => String::new(),
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .expect("client");
        let detected = probe_openai_compatible(&client, "ollama", &format!("http://{address}"))
            .await
            .expect("detected local server");

        assert_eq!(detected.models.len(), 2);
        let qwen = detected
            .models
            .iter()
            .find(|m| m.id == "qwen3:32b")
            .expect("qwen");
        let gemma = detected
            .models
            .iter()
            .find(|m| m.id == "gemma3:1b")
            .expect("gemma");
        assert!(qwen.supports_reasoning);
        assert!(!gemma.supports_reasoning);
        assert_eq!(
            qwen.reasoning_capabilities,
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Separated),
                tags: None,
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            })
        );
        // Non-thinking model → cleared so the silent-drop
        // invariant kicks in.
        assert_eq!(gemma.reasoning_capabilities, None);
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
