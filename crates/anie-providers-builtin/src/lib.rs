//! Built-in provider implementations and shared HTTP/SSE helpers.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod anthropic;
mod http;
mod local;
mod model_discovery;
mod models;
mod ollama_chat;
mod openai;
mod openrouter;
mod sse;
mod tool_schema;
mod util;

pub use anthropic::AnthropicProvider;
pub use http::create_http_client;
pub use local::{LocalServer, detect_local_servers, probe_openai_compatible};
pub use model_discovery::{ModelDiscoveryCache, ModelDiscoveryRequest, discover_models};
pub use models::builtin_models;
pub use ollama_chat::OllamaChatProvider;
pub use openai::OpenAIProvider;
pub use openrouter::{
    apply_openrouter_capabilities, insert_anthropic_cache_control, is_openrouter_target,
    needs_anthropic_cache_control, openrouter_capabilities_for,
};
pub use sse::{SseError, SseEvent, sse_stream};
pub use util::{classify_http_error, parse_retry_after};

use anie_provider::{ApiKind, ProviderRegistry};

/// True when model discovery should prefer Ollama's native
/// `/api/tags` + `/api/show` and convert results to
/// `ApiKind::OllamaChatApi`.
#[must_use]
pub fn is_ollama_native_discovery_target(provider_name: &str, base_url: &str) -> bool {
    provider_name.eq_ignore_ascii_case("ollama")
        || ollama_native_base_url(base_url).contains(":11434")
}

/// Convert an Ollama OpenAI-compatible base URL (`.../v1`) into
/// the root URL expected by native `/api/chat` endpoints.
#[must_use]
pub fn ollama_native_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
}

/// Register the currently implemented built-in providers.
pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register(
        ApiKind::AnthropicMessages,
        Box::new(AnthropicProvider::new()),
    );
    registry.register(ApiKind::OpenAICompletions, Box::new(OpenAIProvider::new()));
    registry.register(ApiKind::OllamaChatApi, Box::new(OllamaChatProvider::new()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_native_discovery_target_accepts_name_or_endpoint_shape() {
        assert!(is_ollama_native_discovery_target(
            "ollama",
            "http://localhost:11434/v1"
        ));
        assert!(is_ollama_native_discovery_target(
            "custom",
            "http://127.0.0.1:11434"
        ));
        assert!(!is_ollama_native_discovery_target(
            "lmstudio",
            "http://localhost:1234/v1"
        ));
    }

    #[test]
    fn ollama_native_base_url_strips_openai_v1_suffix() {
        assert_eq!(
            ollama_native_base_url("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            ollama_native_base_url("http://localhost:11434"),
            "http://localhost:11434"
        );
    }
}
