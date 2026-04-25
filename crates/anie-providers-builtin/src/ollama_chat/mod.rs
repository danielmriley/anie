//! Ollama native `/api/chat` provider.
//!
//! anie-specific (not in pi): pi uses Ollama's OpenAI-compatible
//! endpoint, but that path cannot honor `think: false` or
//! `options.num_ctx`. This module owns Ollama's native NDJSON
//! transport instead of sharing the OpenAI SSE state machine.

use async_stream::try_stream;
use futures::StreamExt;

use anie_protocol::{Message, ToolDef};
use anie_provider::{
    LlmContext, LlmMessage, Model, Provider, ProviderError, ProviderEvent, ProviderStream,
    StreamOptions,
};

mod convert;
mod ndjson;
mod streaming;

use convert::build_request_body;
use ndjson::NdjsonLines;
use streaming::OllamaChatStreamState;

/// Ollama native `/api/chat` provider implementation.
#[derive(Clone)]
pub struct OllamaChatProvider {
    client: reqwest::Client,
}

impl OllamaChatProvider {
    /// Create a new provider using the workspace-shared HTTP client.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: crate::http::shared_http_client()
                .cloned()
                .unwrap_or_else(|_| crate::http::create_http_client()),
        }
    }

    /// Test-only: expose the serialized request body so tests can
    /// assert on outbound `/api/chat` shape without hitting Ollama.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn build_request_body_for_test(
        &self,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> serde_json::Value {
        build_request_body(model, context, options)
    }

    async fn send_request_with_reasoning_retry(
        client: reqwest::Client,
        url: String,
        body: serde_json::Value,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        match Self::send_request(client.clone(), url.clone(), body.clone(), options).await {
            Ok(response) => Ok(response),
            Err(error @ ProviderError::NativeReasoningUnsupported(_)) => {
                let retry_body = body_without_think(body);
                Self::send_request(client, url, retry_body, options)
                    .await
                    .map_err(|_| error)
            }
            Err(error) => Err(error),
        }
    }

    async fn send_request(
        client: reqwest::Client,
        url: String,
        body: serde_json::Value,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        let mut request = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        if let Some(api_key) = &options.api_key {
            request = request.bearer_auth(api_key);
        }
        for (name, value) in &options.headers {
            request = request.header(name, value);
        }
        let response = request
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let retry_after_ms = crate::parse_retry_after(&response);
        let body = response.text().await.unwrap_or_default();
        Err(classify_ollama_error_body(status, &body, retry_after_ms))
    }
}

impl Default for OllamaChatProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for OllamaChatProvider {
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let provider = self.clone();
        let state_model = model.clone();
        let url = format!("{}/api/chat", model.base_url.trim_end_matches('/'));
        let body = build_request_body(model, &context, &options);

        let stream = try_stream! {
            let response = Self::send_request_with_reasoning_retry(provider.client.clone(), url, body, &options).await?;
            yield ProviderEvent::Start;

            let mut lines = NdjsonLines::new(response.bytes_stream());
            let mut state = OllamaChatStreamState::new(&state_model);
            while let Some(line) = lines.next().await {
                for provider_event in state.process_line(&line?)? {
                    yield provider_event;
                }
            }

            if !state.is_finished() {
                for provider_event in state.finish_stream()? {
                    yield provider_event;
                }
            }
        };

        Ok(Box::pin(stream))
    }

    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage> {
        convert::convert_messages(messages)
    }

    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value> {
        crate::tool_schema::openai_function_schema(tools)
    }
}

fn body_without_think(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(object) = body.as_object_mut() {
        object.remove("think");
    }
    body
}

pub(crate) fn classify_ollama_error_body(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::Auth(body.to_string()),
        429 => ProviderError::RateLimited { retry_after_ms },
        _ => {
            let lower = body.to_ascii_lowercase();
            let has_think = lower.contains("think") || lower.contains("thinking");
            let unsupported_or_invalid = lower.contains("unsupported")
                || lower.contains("not supported")
                || lower.contains("does not support")
                || lower.contains("invalid");
            if has_think && unsupported_or_invalid {
                ProviderError::NativeReasoningUnsupported(body.to_string())
            } else if unsupported_or_invalid {
                ProviderError::FeatureUnsupported(body.to_string())
            } else {
                crate::classify_http_error(status, body, retry_after_ms)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anie_provider::{ApiKind, CostPerMillion, ModelCompat};
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;

    fn ollama_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OllamaChatApi,
            base_url: "http://localhost:11434".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        }
    }

    pub(super) fn empty_context() -> LlmContext {
        LlmContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn build_request_body_for_test_exposes_ollama_shape_under_test_utils() {
        let provider = OllamaChatProvider::new();
        let body = provider.build_request_body_for_test(
            &ollama_model(),
            &empty_context(),
            &StreamOptions::default(),
        );

        assert_eq!(body["model"], "qwen3:32b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["options"]["num_ctx"], 32_768);
    }

    #[test]
    fn classify_ollama_error_body_routes_think_wording_to_native_reasoning_unsupported() {
        let error = classify_ollama_error_body(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"think value \"low\" is not supported for this model"}"#,
            None,
        );

        assert!(matches!(
            error,
            ProviderError::NativeReasoningUnsupported(_)
        ));
    }

    #[test]
    fn classify_ollama_error_body_does_not_treat_generic_unsupported_as_reasoning() {
        let error = classify_ollama_error_body(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"error":"images are not supported"}"#,
            None,
        );

        assert!(matches!(error, ProviderError::FeatureUnsupported(_)));
    }

    #[test]
    fn classify_ollama_error_body_routes_401_to_auth_error() {
        let error = classify_ollama_error_body(reqwest::StatusCode::UNAUTHORIZED, "nope", None);

        assert!(matches!(error, ProviderError::Auth(message) if message == "nope"));
    }

    #[test]
    fn classify_ollama_error_body_routes_429_to_rate_limited_with_retry_after() {
        let error = classify_ollama_error_body(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            Some(1_000),
        );

        assert!(matches!(
            error,
            ProviderError::RateLimited {
                retry_after_ms: Some(1_000)
            }
        ));
    }

    async fn capture_request_headers(options: StreamOptions) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut buffer = vec![0_u8; 8192];
            let read = socket.read(&mut buffer).await.expect("read request");
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .expect("write response");
            String::from_utf8_lossy(&buffer[..read]).to_string()
        });

        OllamaChatProvider::send_request(
            reqwest::Client::new(),
            format!("http://{addr}/api/chat"),
            json!({"model": "qwen3:32b"}),
            &options,
        )
        .await
        .expect("send request");

        server.await.expect("server task")
    }

    #[tokio::test]
    async fn request_body_attaches_bearer_auth_when_api_key_present() {
        let request = capture_request_headers(StreamOptions {
            api_key: Some("test-key".into()),
            ..StreamOptions::default()
        })
        .await;

        assert!(request.contains("authorization: Bearer test-key"));
    }

    #[tokio::test]
    async fn request_body_omits_bearer_auth_when_api_key_absent() {
        let request = capture_request_headers(StreamOptions::default()).await;

        assert!(!request.to_ascii_lowercase().contains("authorization:"));
    }

    #[tokio::test]
    async fn request_body_attaches_custom_headers_from_stream_options() {
        let request = capture_request_headers(StreamOptions {
            headers: HashMap::from([("x-test-header".into(), "present".into())]),
            ..StreamOptions::default()
        })
        .await;

        assert!(request.contains("x-test-header: present"));
    }

    async fn capture_reasoning_retry_requests(
        second_response: &'static str,
    ) -> (Result<reqwest::Response, ProviderError>, Vec<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for index in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut buffer = vec![0_u8; 8192];
                let read = socket.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                requests.push(request.clone());
                if index == 0 {
                    let body = r#"{"error":"first think option is not supported"}"#;
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await
                        .expect("write first response");
                } else {
                    socket
                        .write_all(second_response.as_bytes())
                        .await
                        .expect("write second response");
                }
            }
            requests
        });

        let result = OllamaChatProvider::send_request_with_reasoning_retry(
            reqwest::Client::new(),
            format!("http://{addr}/api/chat"),
            json!({
                "model": "qwen3:32b",
                "think": true,
            }),
            &StreamOptions::default(),
        )
        .await;
        let requests = server.await.expect("server task");
        (result, requests)
    }

    #[tokio::test]
    async fn native_reasoning_unsupported_error_triggers_second_attempt_without_think() {
        let (result, requests) = capture_reasoning_retry_requests(
            "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )
        .await;

        result.expect("retry succeeds");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains(r#""think":true"#));
        assert!(!requests[1].contains(r#""think""#));
    }

    #[tokio::test]
    async fn native_reasoning_unsupported_on_second_attempt_surfaces_original_error() {
        let (result, requests) = capture_reasoning_retry_requests(
            "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\n{\"error\":\"second think option is not supported\"}",
        )
        .await;

        assert_eq!(requests.len(), 2);
        let error = result.expect_err("second failure should surface original error");
        assert!(matches!(
            error,
            ProviderError::NativeReasoningUnsupported(message) if message.contains("first think")
        ));
    }
}
