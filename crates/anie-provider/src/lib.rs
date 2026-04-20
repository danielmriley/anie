//! Provider traits, model metadata, and request-resolution contracts for anie-rs.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod api_kind;
mod error;
mod model;
mod options;
mod provider;
mod registry;
mod thinking;

pub use api_kind::ApiKind;
pub use error::ProviderError;
pub use model::{
    CostPerMillion, Model, ModelCompat, ModelInfo, ModelPricing, OpenAICompletionsCompat,
    OpenRouterRouting, ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode,
    ReasoningTags, ReplayCapabilities, ThinkingRequestMode,
};
pub use options::{LlmContext, LlmMessage, ResolvedRequestOptions, StreamOptions};
pub use provider::{Provider, ProviderEvent, ProviderStream, RequestOptionsResolver};
pub use registry::ProviderRegistry;
pub use thinking::ThinkingLevel;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

#[cfg(test)]
mod tests;
