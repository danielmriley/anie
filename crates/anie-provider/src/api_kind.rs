use serde::{Deserialize, Serialize};

/// Supported provider wire protocols.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ApiKind {
    /// Anthropic Messages API.
    AnthropicMessages,
    /// OpenAI-compatible Chat Completions API.
    OpenAICompletions,
    /// OpenAI Responses API.
    OpenAIResponses,
    /// Google Generative AI streaming API.
    GoogleGenerativeAI,
    /// Ollama's native `/api/chat` endpoint.
    ///
    /// anie-specific (not in pi): pi uses Ollama's
    /// OpenAI-compatible endpoint, but that path cannot honor
    /// `think: false` or `options.num_ctx`.
    OllamaChatApi,
}
