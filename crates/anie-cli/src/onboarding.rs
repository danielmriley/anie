use std::io::{self, Write};

use anyhow::{Context, Result};

use anie_auth::save_api_key;
use anie_config::global_config_path;
use anie_providers_builtin::{builtin_models, detect_local_servers};

/// Detect whether this looks like a first run with no config or saved auth.
#[must_use]
pub fn check_first_run() -> bool {
    let config_path = global_config_path();
    let auth_path = anie_auth::auth_file_path();
    config_path.as_deref().is_some_and(|path| !path.exists())
        && auth_path.as_deref().is_some_and(|path| !path.exists())
}

/// Run a minimal first-run onboarding flow.
pub async fn run_onboarding() -> Result<()> {
    println!("Welcome to anie! Let's get you set up.\n");

    let config_path = global_config_path().context("home directory is not available")?;

    let local_servers = detect_local_servers().await;
    if let Some(server) = local_servers.first()
        && let Some(model) = server.models.first()
    {
        println!("✓ Detected local model server: {}", server.name);
        write_config(&config_path, &detected_local_config(model))?;
        println!(
            "\nCreated ~/.anie/config.toml using {} as the default provider.\n",
            server.name
        );
        return Ok(());
    }

    for (provider, env_var) in [
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
    ] {
        if std::env::var(env_var).is_ok() {
            println!("✓ Found {env_var} in the environment.");
            write_config(&config_path, &provider_config(provider))?;
            println!("\nCreated ~/.anie/config.toml with {provider} as the default provider.\n");
            return Ok(());
        }
    }

    println!("No local provider or API key environment variable was found. Choose a provider:\n");
    println!("  1. Anthropic");
    println!("  2. OpenAI");
    println!("  3. Custom OpenAI-compatible endpoint");
    print!("\nSelection [1]: ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut selection = String::new();
    io::stdin()
        .read_line(&mut selection)
        .context("failed to read provider selection")?;
    match selection.trim().parse::<u32>().unwrap_or(1) {
        2 => configure_builtin_provider(&config_path, "openai")?,
        3 => configure_custom_provider(&config_path)?,
        _ => configure_builtin_provider(&config_path, "anthropic")?,
    }

    println!("\n✓ Configuration saved. Starting anie...\n");
    Ok(())
}

fn configure_builtin_provider(config_path: &std::path::Path, provider: &str) -> Result<()> {
    let prompt = match provider {
        "openai" => "Enter your OpenAI API key: ",
        _ => "Enter your Anthropic API key: ",
    };
    let key = rpassword::prompt_password(prompt).context("failed to read API key")?;
    save_api_key(provider, &key)?;
    write_config(config_path, &provider_config(provider))
}

fn configure_custom_provider(config_path: &std::path::Path) -> Result<()> {
    print!("Base URL (for example http://localhost:11434/v1): ");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut base_url = String::new();
    io::stdin()
        .read_line(&mut base_url)
        .context("failed to read base URL")?;
    let base_url = base_url.trim();

    print!("Provider name [custom]: ");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut provider_name = String::new();
    io::stdin()
        .read_line(&mut provider_name)
        .context("failed to read provider name")?;
    let provider_name = if provider_name.trim().is_empty() {
        "custom".to_string()
    } else {
        provider_name.trim().to_string()
    };

    print!("Default model ID: ");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut model_id = String::new();
    io::stdin()
        .read_line(&mut model_id)
        .context("failed to read model ID")?;
    let model_id = model_id.trim();

    let key = rpassword::prompt_password("API key (leave empty for local providers): ")
        .context("failed to read API key")?;
    if !key.is_empty() {
        save_api_key(&provider_name, &key)?;
    }

    write_config(
        config_path,
        &custom_provider_config(&provider_name, model_id, base_url),
    )
}

fn detected_local_config(model: &anie_provider::Model) -> String {
    format!(
        "[model]\nprovider = \"{}\"\nid = \"{}\"\nthinking = \"medium\"\n\n[providers.{}]\nbase_url = \"{}\"\napi = \"OpenAICompletions\"\n[[providers.{}.models]]\nid = \"{}\"\nname = \"{}\"\ncontext_window = {}\nmax_tokens = {}\n",
        model.provider,
        model.id,
        model.provider,
        model.base_url,
        model.provider,
        model.id,
        model.name,
        model.context_window,
        model.max_tokens,
    )
}

fn custom_provider_config(provider_name: &str, model_id: &str, base_url: &str) -> String {
    format!(
        "[model]\nprovider = \"{provider_name}\"\nid = \"{model_id}\"\nthinking = \"medium\"\n\n[providers.{provider_name}]\nbase_url = \"{base_url}\"\napi = \"OpenAICompletions\"\n[[providers.{provider_name}.models]]\nid = \"{model_id}\"\nname = \"{model_id}\"\ncontext_window = 32768\nmax_tokens = 8192\n"
    )
}

fn provider_config(provider: &str) -> String {
    let model = builtin_models()
        .into_iter()
        .find(|model| model.provider == provider)
        .expect("builtin provider should have a default model");
    format!(
        "[model]\nprovider = \"{}\"\nid = \"{}\"\nthinking = \"medium\"\n",
        model.provider, model.id,
    )
}

fn write_config(path: &std::path::Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use anie_provider::{ApiKind, CostPerMillion, Model};

    use super::*;

    fn sample_local_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "qwen3:32b".into(),
            provider: "ollama".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost:11434/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
        }
    }

    #[test]
    fn detected_local_config_uses_medium_thinking_by_default() {
        let config = detected_local_config(&sample_local_model());

        assert!(config.contains("thinking = \"medium\""));
        assert!(!config.contains("thinking = \"off\""));
        assert!(config.contains("provider = \"ollama\""));
        assert!(config.contains("base_url = \"http://localhost:11434/v1\""));
    }

    #[test]
    fn custom_provider_config_uses_medium_thinking_by_default() {
        let config = custom_provider_config("custom", "local-model", "http://localhost:1234/v1");

        assert!(config.contains("thinking = \"medium\""));
        assert!(!config.contains("thinking = \"off\""));
        assert!(config.contains("provider = \"custom\""));
        assert!(config.contains("id = \"local-model\""));
    }
}
