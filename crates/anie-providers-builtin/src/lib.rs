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

/// Register the currently implemented built-in providers.
pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register(
        ApiKind::AnthropicMessages,
        Box::new(AnthropicProvider::new()),
    );
    registry.register(ApiKind::OpenAICompletions, Box::new(OpenAIProvider::new()));
    registry.register(ApiKind::OllamaChatApi, Box::new(OllamaChatProvider::new()));
}
