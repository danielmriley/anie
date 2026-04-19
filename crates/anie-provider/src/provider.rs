use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

use anie_protocol::{AssistantMessage, Message, ToolCall, ToolDef};

use crate::{LlmContext, LlmMessage, Model, ProviderError, ResolvedRequestOptions, StreamOptions};

/// Streaming provider events normalized across provider implementations.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// Stream opened successfully.
    Start,
    /// Text content delta.
    TextDelta(String),
    /// Thinking content delta.
    ThinkingDelta(String),
    /// Tool call started.
    ToolCallStart(ToolCall),
    /// Tool call argument fragment.
    ToolCallDelta { id: String, arguments_delta: String },
    /// Tool call finished.
    ToolCallEnd { id: String },
    /// Final assistant message.
    Done(AssistantMessage),
}

/// The normalized stream type returned by providers.
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send>>;

/// Trait implemented by all providers.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Start a streaming generation request.
    fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError>;

    /// Convert protocol messages into the provider-native format.
    fn convert_messages(&self, messages: &[Message]) -> Vec<LlmMessage>;

    /// Whether assistant thinking blocks should be replayed back to this provider.
    fn includes_thinking_in_replay(&self) -> bool {
        false
    }

    /// Whether the provider's wire format requires an opaque signature
    /// on every replayed thinking block. When `true`, the sanitizer
    /// drops thinking blocks that carry no signature rather than
    /// sending invalid payloads (Anthropic returns a 400 otherwise).
    ///
    /// See docs/api_integrity_plans/01c_serializer_and_sanitizer.md.
    fn requires_thinking_signature(&self) -> bool {
        false
    }

    /// Convert registered tools into the provider-native format.
    fn convert_tools(&self, tools: &[ToolDef]) -> Vec<serde_json::Value>;
}

/// Resolve request-specific auth and routing just before a provider call.
#[async_trait]
pub trait RequestOptionsResolver: Send + Sync {
    /// Resolve per-request options for a given model and context.
    async fn resolve(
        &self,
        model: &Model,
        context: &[Message],
    ) -> Result<ResolvedRequestOptions, ProviderError>;
}
