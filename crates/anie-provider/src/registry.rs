use std::collections::HashMap;

use crate::{ApiKind, LlmContext, Model, Provider, ProviderError, ProviderStream, StreamOptions};

/// Registry of providers keyed by API kind.
pub struct ProviderRegistry {
    providers: HashMap<ApiKind, Box<dyn Provider>>,
}

impl ProviderRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    /// Register a provider for a specific API kind.
    pub fn register(&mut self, api: ApiKind, provider: Box<dyn Provider>) {
        self.providers.insert(api, provider);
    }

    /// Look up a provider by API kind.
    #[must_use]
    pub fn get(&self, api: &ApiKind) -> Option<&dyn Provider> {
        self.providers.get(api).map(Box::as_ref)
    }

    /// Start a stream using the provider implied by the selected model.
    pub fn stream(
        &self,
        model: &Model,
        context: LlmContext,
        options: StreamOptions,
    ) -> Result<ProviderStream, ProviderError> {
        let provider = self.get(&model.api).ok_or_else(|| {
            ProviderError::Request(format!("No provider registered for {:?}", model.api))
        })?;
        provider.stream(model, context, options)
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}
