use anie_config::{CliOverrides, ProviderConfig, load_config_with_paths};
use anie_provider::{ApiKind, ProviderRegistry};
use anie_providers_builtin::register_builtin_providers;

#[test]
fn default_provider_registry_has_builtin_providers() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);

    assert!(
        registry.get(&ApiKind::OpenAICompletions).is_some(),
        "OpenAI provider missing"
    );
    assert!(
        registry.get(&ApiKind::AnthropicMessages).is_some(),
        "Anthropic provider missing"
    );
    assert!(
        registry.get(&ApiKind::GoogleGenerativeAI).is_none(),
        "Google provider should not be registered yet"
    );
}

#[test]
fn custom_provider_config_produces_correct_model_catalog() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[model]
provider = "ollama"
id = "qwen3:32b"

[providers.ollama]
base_url = "http://localhost:11434/v1"
api = "OpenAICompletions"

[[providers.ollama.models]]
id = "qwen3:32b"
name = "Qwen 3 32B"
context_window = 32768
max_tokens = 8192
"#,
    )
    .expect("write config");

    let config =
        load_config_with_paths(Some(&config_path), None, CliOverrides::default()).expect("load");
    let models = anie_config::configured_models(&config);

    assert_eq!(models.len(), 1);
    let model = &models[0];
    assert_eq!(model.id, "qwen3:32b");
    assert_eq!(model.provider, "ollama");
    assert_eq!(model.base_url, "http://localhost:11434/v1");
    assert_eq!(model.api, ApiKind::OpenAICompletions);
    assert_eq!(model.context_window, 32_768);
    assert_eq!(model.max_tokens, 8_192);
}

#[tokio::test]
async fn auth_resolver_with_config_env_var_resolves_key() {
    use anie_auth::AuthResolver;
    use anie_config::AnieConfig;
    use anie_provider::RequestOptionsResolver;

    let env_var = "ANIE_INTEGRATION_TEST_AUTH_KEY";

    temp_env::async_with_vars([(env_var, Some("test-key-value"))], async {
        let mut config = AnieConfig::default();
        config.providers.insert(
            "openai".into(),
            ProviderConfig {
                api_key_env: Some(env_var.into()),
                ..Default::default()
            },
        );

        let resolver = AuthResolver::new(None, config)
            .with_auth_path(Some(std::path::PathBuf::from("/tmp/nonexistent-auth.json")));
        let mut openai_model = anie_integration_tests::helpers::sample_model();
        openai_model.provider = "openai".into();

        let resolved = resolver.resolve(&openai_model, &[]).await.expect("resolve");
        assert_eq!(resolved.api_key.as_deref(), Some("test-key-value"));
    })
    .await;
}
