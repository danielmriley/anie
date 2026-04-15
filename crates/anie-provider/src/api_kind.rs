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
}
