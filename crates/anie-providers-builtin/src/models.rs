use anie_provider::{
    ApiKind, CostPerMillion, Model, ReasoningCapabilities, ReasoningControlMode,
    ReasoningOutputMode,
};

fn native_separated_reasoning() -> Option<ReasoningCapabilities> {
    Some(ReasoningCapabilities {
        control: Some(ReasoningControlMode::Native),
        output: Some(ReasoningOutputMode::Separated),
        tags: None,
    })
}

/// Return the built-in hosted model catalog known at compile time.
#[must_use]
pub fn builtin_models() -> Vec<Model> {
    vec![
        Model {
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            provider: "anthropic".into(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            context_window: 1_000_000,
            max_tokens: 128_000,
            supports_reasoning: true,
            reasoning_capabilities: native_separated_reasoning(),
            supports_images: true,
            cost_per_million: CostPerMillion {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
        },
        Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            provider: "anthropic".into(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            context_window: 1_000_000,
            max_tokens: 128_000,
            supports_reasoning: true,
            reasoning_capabilities: native_separated_reasoning(),
            supports_images: true,
            cost_per_million: CostPerMillion {
                input: 15.0,
                output: 75.0,
                cache_read: 1.5,
                cache_write: 18.75,
            },
        },
        Model {
            id: "claude-haiku-4-5-20251001".into(),
            name: "Claude Haiku 4.5".into(),
            provider: "anthropic".into(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".into(),
            context_window: 200_000,
            max_tokens: 64_000,
            supports_reasoning: true,
            reasoning_capabilities: native_separated_reasoning(),
            supports_images: true,
            cost_per_million: CostPerMillion {
                input: 0.8,
                output: 4.0,
                cache_read: 0.08,
                cache_write: 1.0,
            },
        },
        Model {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".into(),
            context_window: 128_000,
            max_tokens: 16_384,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: true,
            cost_per_million: CostPerMillion {
                input: 2.5,
                output: 10.0,
                cache_read: 1.25,
                cache_write: 0.0,
            },
        },
        Model {
            id: "o4-mini".into(),
            name: "o4-mini".into(),
            provider: "openai".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".into(),
            context_window: 200_000,
            max_tokens: 100_000,
            supports_reasoning: true,
            reasoning_capabilities: native_separated_reasoning(),
            supports_images: true,
            cost_per_million: CostPerMillion {
                input: 1.1,
                output: 4.4,
                cache_read: 0.0,
                cache_write: 0.0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_hosted_models_have_explicit_reasoning_profiles() {
        let models = builtin_models();
        let o4_mini = models
            .iter()
            .find(|model| model.id == "o4-mini")
            .expect("o4-mini model");
        let claude = models
            .iter()
            .find(|model| model.provider == "anthropic")
            .expect("anthropic model");
        let gpt_4o = models
            .iter()
            .find(|model| model.id == "gpt-4o")
            .expect("gpt-4o model");

        assert_eq!(o4_mini.reasoning_capabilities, native_separated_reasoning());
        assert_eq!(claude.reasoning_capabilities, native_separated_reasoning());
        assert_eq!(gpt_4o.reasoning_capabilities, None);
    }
}
