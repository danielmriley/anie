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

    /// Outer retry layer for Ollama model-load failures. Wraps
    /// `send_request_with_reasoning_retry` (which handles the
    /// drop-`think` recovery): on the first attempt's
    /// `ProviderError::ModelLoadResources`, builds a fresh
    /// request body with `num_ctx_override =
    /// Some(suggested_num_ctx)` and tries once more. The second
    /// attempt's outcome propagates verbatim — including a
    /// repeated `ModelLoadResources` (so the user-facing
    /// message in PR 3 reflects the smaller value the inner
    /// retry tried, not the original request's value).
    ///
    /// One retry, not exponential. Each attempt costs an
    /// Ollama model reload (~5-30 s); a second halving on a
    /// system that's already too constrained at half-size is
    /// extremely unlikely to fit at quarter-size and would
    /// just double the wasted reload time.
    ///
    /// See `docs/ollama_load_failure_recovery/README.md` PR 2.
    async fn send_request_with_load_resource_retry(
        client: reqwest::Client,
        url: String,
        model: &Model,
        context: &LlmContext,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        let initial_body = build_request_body(model, context, options);
        match Self::send_request_with_reasoning_retry(
            client.clone(),
            url.clone(),
            initial_body,
            options,
        )
        .await
        {
            Err(ProviderError::ModelLoadResources {
                suggested_num_ctx, ..
            }) => {
                let mut retry_options = options.clone();
                retry_options.num_ctx_override = Some(suggested_num_ctx);
                let retry_body = build_request_body(model, context, &retry_options);
                Self::send_request_with_reasoning_retry(client, url, retry_body, &retry_options)
                    .await
            }
            other => other,
        }
    }

    async fn send_request(
        client: reqwest::Client,
        url: String,
        body: serde_json::Value,
        options: &StreamOptions,
    ) -> Result<reqwest::Response, ProviderError> {
        // Capture the request's num_ctx before consuming the
        // body — `classify_ollama_error_body` uses it to compute
        // the halved suggestion when the response is a
        // memory-limit load failure. Reading the body's
        // `options.num_ctx` here keeps the classifier signature
        // self-contained (it doesn't need a back-reference to
        // `StreamOptions`) and works whether the value came from
        // `model.context_window` or `options.num_ctx_override`.
        let request_num_ctx = body
            .get("options")
            .and_then(|opts| opts.get("num_ctx"))
            .and_then(serde_json::Value::as_u64);
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
        Err(classify_ollama_error_body(
            status,
            &body,
            retry_after_ms,
            request_num_ctx,
        ))
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
        let request_model = model.clone();
        let url = format!("{}/api/chat", model.base_url.trim_end_matches('/'));

        let stream = try_stream! {
            let response = Self::send_request_with_load_resource_retry(
                provider.client.clone(),
                url,
                &request_model,
                &context,
                &options,
            )
            .await?;
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

/// Lower bound on suggested `num_ctx` after halving — keeps
/// `ProviderError::ModelLoadResources::suggested_num_ctx` in the
/// range the `/context-length` slash command will accept (it
/// rejects `< 2048`). Repeated halving in pathological setups
/// floors here rather than driving the suggestion to zero.
const MIN_SUGGESTED_NUM_CTX: u64 = 2_048;

pub(crate) fn classify_ollama_error_body(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
    request_num_ctx: Option<u64>,
) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::Auth(body.to_string()),
        429 => ProviderError::RateLimited { retry_after_ms },
        _ => {
            let lower = body.to_ascii_lowercase();
            // Order: load-failure check first so a body that
            // happens to also contain a generic word like
            // "invalid" doesn't get misclassified. The
            // load-failure patterns are specific to the
            // `requires more system memory` shape (verified
            // empirically against a real local Ollama at
            // qwen3:32b with num_ctx=4_194_304).
            //
            // The check is gated on `request_num_ctx` because
            // the suggestion needs a baseline to halve. When
            // the caller doesn't track the request's `num_ctx`
            // (test harnesses, future non-Ollama callers), the
            // body falls through to the generic classifier and
            // surfaces as `Http { status, body }`.
            if let Some(num_ctx) = request_num_ctx
                && looks_like_load_resource_failure(&lower)
            {
                let suggested_num_ctx = (num_ctx / 2).max(MIN_SUGGESTED_NUM_CTX);
                return ProviderError::ModelLoadResources {
                    body: body.to_string(),
                    suggested_num_ctx,
                };
            }
            // 400 with a body that names a parser failure on the
            // *model's* output (not the request). Observed shapes
            // from Ollama at v0.20.x with Qwen-family models that
            // emit `<tool_call>` XML-style blocks: the Go XML
            // parser fails with `xml syntax error on line N:
            // unexpected EOF` when the model emits a truncated
            // tag. JSON-mode tool calls can produce the matching
            // `json: cannot unmarshal ...` shape. Both are
            // server-side parses of model output — a fresh sample
            // is the natural fix, so a retry is appropriate.
            if status.as_u16() == 400 && looks_like_model_output_parse_failure(&lower) {
                return ProviderError::ModelOutputMalformed(body.to_string());
            }
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

/// Recognize Ollama error bodies that mean "the requested
/// `num_ctx` exceeded available memory at model-load time."
///
/// Verified empirically against a real local Ollama (April
/// 2026, Ollama v0.6.x): `qwen3:32b` with `num_ctx=4_194_304`
/// returns
/// `{"error":"model requires more system memory (56.0 GiB) than is available (50.3 GiB)"}`
/// with HTTP 500. The matcher uses several alternative
/// substrings so a single rewording upstream doesn't break
/// recognition. Each pattern is paired with at least one test
/// case in this module's `tests` submodule.
///
/// Conservative on negative matches: we'd rather surface a
/// real load failure as a generic `Http { 500, body }` than
/// wrongly classify an unrelated 500 as a load failure and
/// drop the user into an "/context-length 16384" message that
/// won't help them.
fn looks_like_load_resource_failure(body_lower: &str) -> bool {
    body_lower.contains("requires more system memory")
        || body_lower.contains("more system memory")
        || body_lower.contains("exceeds available memory")
        || body_lower.contains("failed to load model")
        // Belt-and-suspenders for variant phrasings that pair
        // "memory" with "available" — covers wordings like
        // "X bytes required, Y bytes available".
        || (body_lower.contains("memory") && body_lower.contains("available"))
}

/// Recognize Ollama 400 bodies that mean "your model emitted
/// output that I, the server, couldn't parse." These are
/// transient — a fresh sample at the same context will
/// almost certainly produce different tokens — so the caller
/// classifies the error as `ModelOutputMalformed` and the
/// retry policy can attempt the request again.
///
/// Verified empirically (April 2026, Ollama v0.20.x with
/// `qwen3.6:latest` at 65 K context) — the model emitted a
/// truncated `<tool_call>` block and Ollama's Go XML parser
/// returned `xml syntax error on line 5: unexpected EOF`
/// in the body of an HTTP 400. The match list includes both
/// the XML-parser shape and the JSON-parser shape Ollama
/// surfaces when running models that emit JSON tool calls.
///
/// Conservative on positive matches: a 400 with `invalid model`
/// or `bad request` should not be misclassified as a parse
/// failure. The substrings here name parser-specific failure
/// modes; nothing else uses the same wording.
fn looks_like_model_output_parse_failure(body_lower: &str) -> bool {
    body_lower.contains("xml syntax error")
        || body_lower.contains("xml: ")
        || body_lower.contains("json: cannot unmarshal")
        || body_lower.contains("unexpected eof")
        || body_lower.contains("parse error in tool call")
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
            None,
        );

        assert!(matches!(error, ProviderError::FeatureUnsupported(_)));
    }

    #[test]
    fn classify_ollama_error_body_routes_401_to_auth_error() {
        let error =
            classify_ollama_error_body(reqwest::StatusCode::UNAUTHORIZED, "nope", None, None);

        assert!(matches!(error, ProviderError::Auth(message) if message == "nope"));
    }

    #[test]
    fn classify_ollama_error_body_routes_429_to_rate_limited_with_retry_after() {
        let error = classify_ollama_error_body(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            Some(1_000),
            None,
        );

        assert!(matches!(
            error,
            ProviderError::RateLimited {
                retry_after_ms: Some(1_000)
            }
        ));
    }

    #[test]
    fn classify_ollama_error_body_recognizes_requires_more_system_memory() {
        // Verbatim body captured from a real Ollama instance
        // (qwen3:32b at num_ctx=4_194_304 → HTTP 500). PR 1
        // empirical-verification checklist documents the probe
        // that produced this payload.
        let body = r#"{"error":"model requires more system memory (56.0 GiB) than is available (50.3 GiB)"}"#;
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body,
            None,
            Some(4_194_304),
        );
        match error {
            ProviderError::ModelLoadResources {
                body: captured,
                suggested_num_ctx,
            } => {
                assert_eq!(captured, body, "verbatim body must round-trip");
                assert_eq!(suggested_num_ctx, 2_097_152, "halved from 4_194_304");
            }
            other => panic!("expected ModelLoadResources, got {other:?}"),
        }
    }

    #[test]
    fn classify_ollama_error_body_recognizes_failed_to_load_model() {
        // Anticipated alternate wording. The matcher uses
        // multiple substring alternatives so future Ollama
        // releases that reword the error don't break
        // recognition.
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"failed to load model: not enough memory"}"#,
            None,
            Some(131_072),
        );
        assert!(matches!(error, ProviderError::ModelLoadResources { .. }));
    }

    #[test]
    fn classify_ollama_error_body_recognizes_exceeds_available_memory() {
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"context size exceeds available memory"}"#,
            None,
            Some(262_144),
        );
        assert!(matches!(error, ProviderError::ModelLoadResources { .. }));
    }

    #[test]
    fn classify_ollama_error_body_does_not_misclassify_unrelated_500() {
        // Negative: a generic 500 unrelated to memory must NOT
        // become a load-resource failure. False positives here
        // would surface "/context-length 16384" hints on
        // problems the user can't fix that way.
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"internal server error"}"#,
            None,
            Some(40_960),
        );
        assert!(
            matches!(error, ProviderError::Http { status: 500, .. }),
            "unrelated 500 must stay generic Http; got {error:?}"
        );
    }

    #[test]
    fn classify_ollama_error_body_does_not_misclassify_404_not_found() {
        // Verbatim body captured from a real Ollama probe with
        // a non-existent model name. Must NOT route to
        // ModelLoadResources — the failure mode is "model not
        // pulled," not "num_ctx too big."
        let error = classify_ollama_error_body(
            reqwest::StatusCode::NOT_FOUND,
            r#"{"error":"model 'nope-not-a-model:1b' not found"}"#,
            None,
            Some(40_960),
        );
        assert!(
            matches!(error, ProviderError::Http { status: 404, .. }),
            "model-not-found must stay Http {{ 404, .. }}; got {error:?}"
        );
    }

    #[test]
    fn load_failure_body_is_not_classified_as_context_overflow() {
        // Boundary test against the existing
        // `classify_http_error` overflow detector
        // (`util.rs:20-32`), which fires only on HTTP 400 with a
        // body containing "context" or "token". A 500 with the
        // load-failure body must never reach that detector.
        let load_failure_body = r#"{"error":"model requires more system memory (56.0 GiB) than is available (50.3 GiB)"}"#;
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            load_failure_body,
            None,
            Some(4_194_304),
        );
        assert!(
            matches!(error, ProviderError::ModelLoadResources { .. }),
            "load-failure must classify as ModelLoadResources, never as ContextOverflow; got {error:?}"
        );
        assert!(
            !matches!(error, ProviderError::ContextOverflow(_)),
            "explicit guard against the wrong direction"
        );
    }

    /// Ollama returns HTTP 400 with `xml syntax error on line N:
    /// unexpected EOF` when a Qwen-family model emits a truncated
    /// `<tool_call>` block. Anie should classify this as
    /// `ModelOutputMalformed` (auto-retryable) rather than the
    /// generic `Http { 400, .. }` (terminal) — the model's output
    /// is what's wrong, not the request.
    #[test]
    fn classify_ollama_error_body_routes_xml_syntax_to_model_output_malformed() {
        let body = r#"{"error":"xml syntax error on line 5: unexpected EOF"}"#;
        let error =
            classify_ollama_error_body(reqwest::StatusCode::BAD_REQUEST, body, None, Some(65_536));
        assert!(
            matches!(error, ProviderError::ModelOutputMalformed(_)),
            "expected ModelOutputMalformed, got: {error:?}",
        );
    }

    /// JSON-mode tool-call parse failures show the same shape
    /// from a different parser. The classifier should catch both
    /// XML and JSON parser-failure phrasings.
    #[test]
    fn classify_ollama_error_body_routes_json_parse_to_model_output_malformed() {
        let body = r#"{"error":"json: cannot unmarshal string into Go struct field"}"#;
        let error = classify_ollama_error_body(reqwest::StatusCode::BAD_REQUEST, body, None, None);
        assert!(
            matches!(error, ProviderError::ModelOutputMalformed(_)),
            "expected ModelOutputMalformed, got: {error:?}",
        );
    }

    /// Negative case: a 400 that's a real client-side bad
    /// request — `invalid model name`, etc. — must continue to
    /// classify through the existing unsupported/feature path,
    /// not get swallowed by the new model-output-parse branch.
    #[test]
    fn classify_ollama_error_body_does_not_misclassify_invalid_model_as_output_malformed() {
        let body = r#"{"error":"invalid model name"}"#;
        let error = classify_ollama_error_body(reqwest::StatusCode::BAD_REQUEST, body, None, None);
        // "invalid" is in the body, so this routes to the
        // unsupported/feature path, not to ModelOutputMalformed.
        assert!(
            matches!(error, ProviderError::FeatureUnsupported(_)),
            "expected FeatureUnsupported (existing behavior preserved), got: {error:?}",
        );
    }

    /// The model-output-parse branch is gated on status 400.
    /// A 500 with similar body wording should not slip through.
    #[test]
    fn classify_ollama_error_body_only_routes_400_to_model_output_malformed() {
        let body = r#"{"error":"xml syntax error: unexpected EOF"}"#;
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body,
            None,
            None,
        );
        assert!(
            !matches!(error, ProviderError::ModelOutputMalformed(_)),
            "ModelOutputMalformed must be 400-only; got: {error:?}",
        );
    }

    #[test]
    fn classify_ollama_error_body_falls_back_to_http_when_request_num_ctx_unknown() {
        // Without a `request_num_ctx` baseline, the suggestion
        // can't be computed; the body falls through to the
        // generic classifier. This guards against future
        // refactors that quietly thread a default like `Some(0)`
        // and end up with `suggested_num_ctx = 2048` for every
        // generic 500.
        let body = r#"{"error":"model requires more system memory (56.0 GiB) than is available (50.3 GiB)"}"#;
        let error = classify_ollama_error_body(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            body,
            None,
            None,
        );
        assert!(
            matches!(error, ProviderError::Http { status: 500, .. }),
            "no request_num_ctx → no suggestion → fall through to Http; got {error:?}"
        );
    }

    #[test]
    fn model_load_resources_suggested_num_ctx_is_half_of_requested() {
        for (requested, expected) in [
            (262_144_u64, 131_072_u64),
            (40_960, 20_480),
            (16_384, 8_192),
            (4_096, 2_048),
        ] {
            let error = classify_ollama_error_body(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"model requires more system memory"}"#,
                None,
                Some(requested),
            );
            match error {
                ProviderError::ModelLoadResources {
                    suggested_num_ctx, ..
                } => assert_eq!(
                    suggested_num_ctx, expected,
                    "halving from {requested} should yield {expected}"
                ),
                other => panic!("expected ModelLoadResources for {requested}, got {other:?}"),
            }
        }
    }

    #[test]
    fn model_load_resources_suggested_num_ctx_floors_below_2048_at_2048() {
        // Pathological: a requested value that halves below the
        // /context-length minimum must floor at 2048 so the
        // suggested value is still acceptable to the slash
        // command. Otherwise a user with already-tiny num_ctx
        // would see a hint they can't act on.
        for requested in [3_000_u64, 2_500, 2_048, 1_024, 100, 1] {
            let error = classify_ollama_error_body(
                reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":"model requires more system memory"}"#,
                None,
                Some(requested),
            );
            match error {
                ProviderError::ModelLoadResources {
                    suggested_num_ctx, ..
                } => assert!(
                    suggested_num_ctx >= 2_048,
                    "suggestion must floor at 2048; got {suggested_num_ctx} from requested {requested}"
                ),
                other => panic!("expected ModelLoadResources, got {other:?}"),
            }
        }
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

    /// Drive `send_request_with_load_resource_retry` against a
    /// 2-attempt mock. First attempt always returns the
    /// captured Ollama load-failure body (HTTP 500); the second
    /// attempt returns whatever `second_response` specifies.
    /// Captures the request body bytes from both attempts so
    /// tests can assert that `num_ctx` halved between them.
    async fn capture_load_resource_retry_requests(
        second_response: &'static str,
        initial_num_ctx: u64,
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
                    let body = r#"{"error":"model requires more system memory (56.0 GiB) than is available (50.3 GiB)"}"#;
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
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

        let mut model = ollama_model();
        model.context_window = initial_num_ctx;
        // Avoid the `think` field appearing in the request body
        // so this test is purely about num_ctx retry semantics
        // (the existing reasoning-retry harness already covers
        // think dropping). `reasoning_capabilities = None` means
        // build_request_body skips the `think` field.
        model.reasoning_capabilities = None;
        let context = empty_context();
        let result = OllamaChatProvider::send_request_with_load_resource_retry(
            reqwest::Client::new(),
            format!("http://{addr}/api/chat"),
            &model,
            &context,
            &StreamOptions::default(),
        )
        .await;
        let requests = server.await.expect("server task");
        (result, requests)
    }

    fn extract_num_ctx_from_request(raw_request: &str) -> u64 {
        let body_start = raw_request
            .find("\r\n\r\n")
            .map(|idx| idx + 4)
            .expect("HTTP body separator");
        let body: serde_json::Value =
            serde_json::from_str(&raw_request[body_start..]).expect("JSON body");
        body["options"]["num_ctx"]
            .as_u64()
            .expect("options.num_ctx field")
    }

    #[tokio::test]
    async fn ollama_load_failure_recovered_by_halved_retry_streams_normally() {
        // First attempt at num_ctx=131_072 returns load failure;
        // second attempt at num_ctx=65_536 (half) succeeds. The
        // wrapper must propagate the success to the caller and
        // make exactly two requests.
        let (result, requests) = capture_load_resource_retry_requests(
            "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            131_072,
        )
        .await;

        result.expect("retry should recover via halved num_ctx");
        assert_eq!(requests.len(), 2, "exactly one retry, no exponential decay");
        assert_eq!(
            extract_num_ctx_from_request(&requests[0]),
            131_072,
            "first attempt uses the initial num_ctx"
        );
        assert_eq!(
            extract_num_ctx_from_request(&requests[1]),
            65_536,
            "second attempt uses half"
        );
    }

    #[tokio::test]
    async fn ollama_load_failure_triggers_one_halved_retry_then_surfaces() {
        // Both attempts return load failure; the wrapper must
        // propagate the SECOND attempt's error so the user-facing
        // message in PR 3 reflects the smaller suggested value
        // (here: 65_536 / 2 = 32_768).
        let (result, requests) = capture_load_resource_retry_requests(
            "HTTP/1.1 500 Internal Server Error\r\nconnection: close\r\n\r\n{\"error\":\"model requires more system memory after halving too\"}",
            131_072,
        )
        .await;

        assert_eq!(
            requests.len(),
            2,
            "no third attempt — one halved retry only"
        );
        let error = result.expect_err("second failure must surface as Err");
        match error {
            ProviderError::ModelLoadResources {
                suggested_num_ctx, ..
            } => {
                assert_eq!(
                    suggested_num_ctx, 32_768,
                    "must reflect the halved attempt's suggestion (65_536/2), not the original"
                );
            }
            other => panic!(
                "second-attempt failure should propagate as ModelLoadResources, got {other:?}"
            ),
        }
    }

    #[tokio::test]
    async fn ollama_load_failure_retry_floors_suggested_num_ctx_at_minimum() {
        // Pathological setup: requested num_ctx is already so
        // small that the first failure suggests halving below
        // the 2_048 floor. The retry should still happen
        // (with num_ctx=2_048, the floor) rather than refuse
        // outright.
        let (result, requests) = capture_load_resource_retry_requests(
            "HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            3_000,
        )
        .await;

        result.expect("retry should still proceed at the floor");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            extract_num_ctx_from_request(&requests[1]),
            2_048,
            "halved from 3_000 floors at MIN_SUGGESTED_NUM_CTX"
        );
    }

    #[tokio::test]
    async fn ollama_non_load_failure_does_not_trigger_halved_retry() {
        // Negative test: a non-load-failure (e.g. 401) on the
        // first attempt must NOT trigger a halved-num_ctx
        // retry. Only `ModelLoadResources` does.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server addr");
        let server = tokio::spawn(async move {
            let mut count = 0;
            while let Ok((mut socket, _)) = listener.accept().await {
                count += 1;
                let mut buffer = vec![0_u8; 8192];
                let _ = socket.read(&mut buffer).await;
                socket
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope")
                    .await
                    .ok();
            }
            count
        });

        let mut model = ollama_model();
        model.context_window = 131_072;
        model.reasoning_capabilities = None;
        let context = empty_context();
        let result = OllamaChatProvider::send_request_with_load_resource_retry(
            reqwest::Client::new(),
            format!("http://{addr}/api/chat"),
            &model,
            &context,
            &StreamOptions::default(),
        )
        .await;

        let error = result.expect_err("auth error must surface");
        assert!(
            matches!(error, ProviderError::Auth(_)),
            "auth error must NOT route through num_ctx retry; got {error:?}"
        );
        // We can't reliably count exactly-one accept on the mock
        // because the server task was aborted on test exit, but
        // the absence of a halved retry is verified by the typed
        // error (Auth, not ModelLoadResources).
        drop(server);
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
