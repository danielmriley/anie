//! TOML configuration loading, merging, and project-context discovery.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod mutation;

use anie_provider::{
    ApiKind, CostPerMillion, Model, ReasoningCapabilities, ReasoningControlMode,
    ReasoningOutputMode, ReasoningTags, ThinkingLevel, ThinkingRequestMode,
};

pub use mutation::ConfigMutator;

/// Fully-resolved application configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnieConfig {
    /// Default model configuration.
    pub model: ModelConfig,
    /// Provider-specific configuration.
    pub providers: HashMap<String, ProviderConfig>,
    /// Compaction settings.
    pub compaction: CompactionConfig,
    /// Project-context discovery limits.
    pub context: ContextConfig,
}

impl Default for AnieConfig {
    fn default() -> Self {
        Self {
            model: ModelConfig::default(),
            providers: HashMap::new(),
            compaction: CompactionConfig::default(),
            context: ContextConfig::default(),
        }
    }
}

/// Default model selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    /// Default provider name.
    pub provider: String,
    /// Default model identifier.
    pub id: String,
    /// Default thinking level.
    pub thinking: ThinkingLevel,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            thinking: ThinkingLevel::Medium,
        }
    }
}

/// Provider-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ProviderConfig {
    /// Environment variable used to look up the API key.
    pub api_key_env: Option<String>,
    /// Optional base URL override.
    pub base_url: Option<String>,
    /// Optional API kind override.
    pub api: Option<ApiKind>,
    /// Optional custom model catalog for this provider.
    #[serde(default)]
    pub models: Vec<CustomModelConfig>,
}

/// A custom model declared in configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CustomModelConfig {
    /// Model identifier.
    pub id: String,
    /// Human-readable model name.
    pub name: String,
    /// Context window size.
    pub context_window: u64,
    /// Max output tokens.
    pub max_tokens: u64,
    /// Whether the model supports reasoning.
    #[serde(default)]
    pub supports_reasoning: bool,
    /// Optional richer reasoning control metadata.
    #[serde(default)]
    pub reasoning_control: Option<ReasoningControlMode>,
    /// Optional richer reasoning output metadata.
    #[serde(default)]
    pub reasoning_output: Option<ReasoningOutputMode>,
    /// Optional explicit opening tag for tagged reasoning.
    #[serde(default)]
    pub reasoning_tag_open: Option<String>,
    /// Optional explicit closing tag for tagged reasoning.
    #[serde(default)]
    pub reasoning_tag_close: Option<String>,
    /// Optional explicit request-shape for thinking support.
    #[serde(default)]
    pub thinking_request_mode: Option<ThinkingRequestMode>,
    /// Whether the model supports images.
    #[serde(default)]
    pub supports_images: bool,
}

/// Context-compaction settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionConfig {
    /// Whether compaction is enabled.
    pub enabled: bool,
    /// Reserved token budget.
    pub reserve_tokens: u64,
    /// Recent-token budget to keep verbatim.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

/// Project-context file discovery settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextConfig {
    /// Filenames to search for while walking from CWD upward.
    pub filenames: Vec<String>,
    /// Maximum bytes loaded from any single file.
    pub max_file_bytes: u64,
    /// Maximum total bytes loaded across all files.
    pub max_total_bytes: u64,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            filenames: vec!["AGENTS.md".into(), "CLAUDE.md".into()],
            max_file_bytes: 32_768,
            max_total_bytes: 65_536,
        }
    }
}

/// CLI configuration overrides applied after file-based config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CliOverrides {
    /// Override the default model ID.
    pub model: Option<String>,
    /// Override the default provider name.
    pub provider: Option<String>,
    /// Override the default thinking level.
    pub thinking: Option<ThinkingLevel>,
}

use std::time::SystemTime;

/// A project-context file loaded into the system prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextFile {
    /// Absolute path to the source file.
    pub path: PathBuf,
    /// Loaded file contents (possibly truncated).
    pub contents: String,
    /// Whether the file was truncated by config caps.
    pub truncated: bool,
    /// File modification time at the point of reading.
    pub mtime: Option<SystemTime>,
}

/// Return the default global config path.
#[must_use]
pub fn global_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".anie/config.toml"))
}

/// Walk upward from `start` to find a project config file.
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    for directory in start.ancestors() {
        let candidate = directory.join(".anie/config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Determine the preferred config file for writes from the current working tree.
#[must_use]
pub fn preferred_write_target(cwd: &Path) -> Option<PathBuf> {
    find_project_config(cwd).or_else(global_config_path)
}

/// Load configuration from the standard global/project paths and apply CLI overrides.
pub fn load_config(cli_overrides: CliOverrides) -> Result<AnieConfig> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let global = global_config_path();
    let project = find_project_config(&cwd);
    load_config_with_paths(global.as_deref(), project.as_deref(), cli_overrides)
}

/// Load configuration from explicit file paths and apply CLI overrides.
pub fn load_config_with_paths(
    global_path: Option<&Path>,
    project_path: Option<&Path>,
    cli_overrides: CliOverrides,
) -> Result<AnieConfig> {
    let mut config = AnieConfig::default();

    if let Some(global_path) = global_path
        && global_path.is_file()
    {
        let partial = load_partial_config(global_path)?;
        merge_partial_config(&mut config, partial);
    }

    if let Some(project_path) = project_path
        && project_path.is_file()
    {
        let partial = load_partial_config(project_path)?;
        merge_partial_config(&mut config, partial);
    }

    apply_cli_overrides(&mut config, cli_overrides);
    Ok(config)
}

/// Ensure the global config file exists, creating a commented template when missing.
pub fn ensure_global_config_exists() -> Result<PathBuf> {
    let path = global_config_path().context("home directory is not available")?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&path, default_config_template())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(path)
}

/// Build a custom model catalog from provider config.
#[must_use]
pub fn configured_models(config: &AnieConfig) -> Vec<Model> {
    let mut models = Vec::new();
    for (provider_name, provider_config) in &config.providers {
        let Some(base_url) = &provider_config.base_url else {
            continue;
        };
        let api = provider_config.api.unwrap_or(ApiKind::OpenAICompletions);
        for model in &provider_config.models {
            models.push(Model {
                id: model.id.clone(),
                name: model.name.clone(),
                provider: provider_name.clone(),
                api,
                base_url: base_url.clone(),
                context_window: model.context_window,
                max_tokens: model.max_tokens,
                supports_reasoning: model.supports_reasoning,
                reasoning_capabilities: custom_model_reasoning_capabilities(model),
                supports_images: model.supports_images,
                cost_per_million: CostPerMillion::zero(),
            });
        }
    }
    models
}

fn custom_model_reasoning_capabilities(model: &CustomModelConfig) -> Option<ReasoningCapabilities> {
    let tags = match (&model.reasoning_tag_open, &model.reasoning_tag_close) {
        (Some(open), Some(close)) => Some(ReasoningTags {
            open: open.clone(),
            close: close.clone(),
        }),
        _ => None,
    };

    if model.reasoning_control.is_none()
        && model.reasoning_output.is_none()
        && tags.is_none()
        && model.thinking_request_mode.is_none()
    {
        None
    } else {
        Some(ReasoningCapabilities {
            control: model.reasoning_control,
            output: model.reasoning_output,
            tags,
            request_mode: model.thinking_request_mode,
        })
    }
}

/// Load context files from the current project hierarchy while respecting config caps.
pub fn collect_context_files(cwd: &Path, config: &ContextConfig) -> Result<Vec<ContextFile>> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut total_loaded = 0u64;

    for directory in cwd.ancestors() {
        for filename in &config.filenames {
            let candidate = directory.join(filename);
            if !candidate.is_file() || !seen.insert(candidate.clone()) {
                continue;
            }
            if total_loaded >= config.max_total_bytes {
                return Ok(files);
            }

            let available = config.max_total_bytes.saturating_sub(total_loaded);
            if available == 0 {
                return Ok(files);
            }
            let bytes = fs::read(&candidate)
                .with_context(|| format!("failed to read context file {}", candidate.display()))?;
            let per_file_limit = config.max_file_bytes.min(available);
            let truncated = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > per_file_limit;
            let kept = &bytes[..usize::try_from(per_file_limit)
                .unwrap_or(usize::MAX)
                .min(bytes.len())];
            let mut contents = String::from_utf8_lossy(kept).into_owned();
            if truncated {
                contents.push_str("\n[truncated due to context limits]");
            }

            total_loaded += u64::try_from(kept.len()).unwrap_or(0);
            files.push(ContextFile {
                path: candidate.clone(),
                contents,
                truncated,
                mtime: fs::metadata(&candidate)
                    .and_then(|m| m.modified())
                    .ok(),
            });
        }
    }

    Ok(files)
}

/// Return the default config template written on first run.
#[must_use]
pub fn default_config_template() -> &'static str {
    "# anie-rs configuration\n\n# Default model\n# [model]\n# provider = \"openai\"\n# id = \"gpt-4o\"\n# thinking = \"medium\"\n\n# Provider settings\n# [providers.openai]\n# api_key_env = \"OPENAI_API_KEY\"\n\n# Custom local OpenAI-compatible provider\n# [providers.ollama]\n# base_url = \"http://localhost:11434/v1\"\n# api = \"OpenAICompletions\"\n# [[providers.ollama.models]]\n# id = \"qwen3:32b\"\n# name = \"Qwen 3 32B\"\n# context_window = 32768\n# max_tokens = 8192\n# thinking_request_mode = \"ReasoningEffort\"\n\n# Compaction settings\n# [compaction]\n# enabled = true\n# reserve_tokens = 16384\n# keep_recent_tokens = 20000\n\n# Project context files\n# [context]\n# filenames = [\"AGENTS.md\", \"CLAUDE.md\"]\n# max_file_bytes = 32768\n# max_total_bytes = 65536\n"
}

fn load_partial_config(path: &Path) -> Result<PartialAnieConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

fn merge_partial_config(config: &mut AnieConfig, partial: PartialAnieConfig) {
    if let Some(model) = partial.model {
        if let Some(provider) = model.provider {
            config.model.provider = provider;
        }
        if let Some(id) = model.id {
            config.model.id = id;
        }
        if let Some(thinking) = model.thinking {
            config.model.thinking = thinking;
        }
    }

    for (provider_name, partial_provider) in partial.providers {
        let provider = config.providers.entry(provider_name).or_default();
        if let Some(api_key_env) = partial_provider.api_key_env {
            provider.api_key_env = Some(api_key_env);
        }
        if let Some(base_url) = partial_provider.base_url {
            provider.base_url = Some(base_url);
        }
        if let Some(api) = partial_provider.api {
            provider.api = Some(api);
        }
        if let Some(models) = partial_provider.models {
            provider.models = models;
        }
    }

    if let Some(compaction) = partial.compaction {
        if let Some(enabled) = compaction.enabled {
            config.compaction.enabled = enabled;
        }
        if let Some(reserve_tokens) = compaction.reserve_tokens {
            config.compaction.reserve_tokens = reserve_tokens;
        }
        if let Some(keep_recent_tokens) = compaction.keep_recent_tokens {
            config.compaction.keep_recent_tokens = keep_recent_tokens;
        }
    }

    if let Some(context) = partial.context {
        if let Some(filenames) = context.filenames {
            config.context.filenames = filenames;
        }
        if let Some(max_file_bytes) = context.max_file_bytes {
            config.context.max_file_bytes = max_file_bytes;
        }
        if let Some(max_total_bytes) = context.max_total_bytes {
            config.context.max_total_bytes = max_total_bytes;
        }
    }
}

fn apply_cli_overrides(config: &mut AnieConfig, overrides: CliOverrides) {
    if let Some(provider) = overrides.provider {
        config.model.provider = provider;
    }
    if let Some(model) = overrides.model {
        config.model.id = model;
    }
    if let Some(thinking) = overrides.thinking {
        config.model.thinking = thinking;
    }
}

#[derive(Debug, Default, Deserialize)]
struct PartialAnieConfig {
    #[serde(default)]
    model: Option<PartialModelConfig>,
    #[serde(default)]
    providers: HashMap<String, PartialProviderConfig>,
    #[serde(default)]
    compaction: Option<PartialCompactionConfig>,
    #[serde(default)]
    context: Option<PartialContextConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialModelConfig {
    provider: Option<String>,
    id: Option<String>,
    thinking: Option<ThinkingLevel>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialProviderConfig {
    api_key_env: Option<String>,
    base_url: Option<String>,
    api: Option<ApiKind>,
    models: Option<Vec<CustomModelConfig>>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialCompactionConfig {
    enabled: Option<bool>,
    reserve_tokens: Option<u64>,
    keep_recent_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialContextConfig {
    filenames: Option<Vec<String>>,
    max_file_bytes: Option<u64>,
    max_total_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_minimal_config() {
        let config: PartialAnieConfig =
            toml::from_str("[model]\nid = \"gpt-4o\"\n").expect("parse config");
        assert_eq!(
            config.model.and_then(|model| model.id),
            Some("gpt-4o".into())
        );
    }

    #[test]
    fn defaults_fill_missing_fields() {
        let config =
            load_config_with_paths(None, None, CliOverrides::default()).expect("load defaults");
        assert_eq!(config.model.provider, "openai");
        assert_eq!(config.context.max_total_bytes, 65_536);
    }

    #[test]
    fn parses_full_config() {
        let config: PartialAnieConfig = toml::from_str(
            r#"
            [model]
            provider = "openai"
            id = "o4-mini"
            thinking = "High"

            [providers.local]
            api_key_env = "LOCAL_LLM_KEY"
            base_url = "http://localhost:8080/v1"
            api = "OpenAICompletions"

            [[providers.local.models]]
            id = "qwen"
            name = "Qwen"
            context_window = 32768
            max_tokens = 8192
            supports_reasoning = true
            reasoning_control = "Native"
            reasoning_output = "Tagged"
            reasoning_tag_open = "<think>"
            reasoning_tag_close = "</think>"
            thinking_request_mode = "NestedReasoning"
            "#,
        )
        .expect("parse config");
        assert_eq!(
            config.model.and_then(|model| model.id),
            Some("o4-mini".into())
        );
        assert_eq!(
            config
                .providers
                .get("local")
                .and_then(|provider| provider.base_url.as_deref()),
            Some("http://localhost:8080/v1")
        );
        let model = config
            .providers
            .get("local")
            .and_then(|provider| provider.models.as_ref())
            .and_then(|models| models.first())
            .expect("custom model");
        assert_eq!(model.reasoning_control, Some(ReasoningControlMode::Native));
        assert_eq!(model.reasoning_output, Some(ReasoningOutputMode::Tagged));
        assert_eq!(model.reasoning_tag_open.as_deref(), Some("<think>"));
        assert_eq!(model.reasoning_tag_close.as_deref(), Some("</think>"));
        assert_eq!(
            model.thinking_request_mode,
            Some(ThinkingRequestMode::NestedReasoning)
        );
    }

    #[test]
    fn layer_merging_applies_global_then_project_then_cli() {
        let tempdir = tempdir().expect("tempdir");
        let global_path = tempdir.path().join("global.toml");
        let project_path = tempdir.path().join("project.toml");
        fs::write(
            &global_path,
            "[model]\nprovider = \"openai\"\nid = \"gpt-4o\"\nthinking = \"Low\"\n",
        )
        .expect("write global config");
        fs::write(
            &project_path,
            "[model]\nid = \"o4-mini\"\n[context]\nmax_total_bytes = 1024\n",
        )
        .expect("write project config");

        let config = load_config_with_paths(
            Some(&global_path),
            Some(&project_path),
            CliOverrides {
                provider: Some("local".into()),
                ..CliOverrides::default()
            },
        )
        .expect("load merged config");

        assert_eq!(config.model.provider, "local");
        assert_eq!(config.model.id, "o4-mini");
        assert_eq!(config.model.thinking, ThinkingLevel::Low);
        assert_eq!(config.context.max_total_bytes, 1024);
    }

    #[test]
    fn configured_models_include_reasoning_capabilities_from_custom_provider_entries() {
        let mut config = AnieConfig::default();
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                api_key_env: None,
                base_url: Some("http://localhost:11434/v1".into()),
                api: Some(ApiKind::OpenAICompletions),
                models: vec![CustomModelConfig {
                    id: "qwen3:32b".into(),
                    name: "Qwen 3 32B".into(),
                    context_window: 32_768,
                    max_tokens: 8_192,
                    supports_reasoning: false,
                    reasoning_control: Some(ReasoningControlMode::Prompt),
                    reasoning_output: Some(ReasoningOutputMode::Tagged),
                    reasoning_tag_open: Some("<think>".into()),
                    reasoning_tag_close: Some("</think>".into()),
                    thinking_request_mode: Some(ThinkingRequestMode::ReasoningEffort),
                    supports_images: false,
                }],
            },
        );

        let models = configured_models(&config);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider, "local");
        assert_eq!(models[0].base_url, "http://localhost:11434/v1");
        assert_eq!(
            models[0].reasoning_capabilities,
            Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Prompt),
                output: Some(ReasoningOutputMode::Tagged),
                tags: Some(ReasoningTags {
                    open: "<think>".into(),
                    close: "</think>".into(),
                }),
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            })
        );
    }

    #[test]
    fn configured_models_are_built_from_custom_provider_entries() {
        let mut config = AnieConfig::default();
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                api_key_env: None,
                base_url: Some("http://localhost:11434/v1".into()),
                api: Some(ApiKind::OpenAICompletions),
                models: vec![CustomModelConfig {
                    id: "qwen3:32b".into(),
                    name: "Qwen 3 32B".into(),
                    context_window: 32_768,
                    max_tokens: 8_192,
                    supports_reasoning: false,
                    reasoning_control: None,
                    reasoning_output: None,
                    reasoning_tag_open: None,
                    reasoning_tag_close: None,
                    thinking_request_mode: None,
                    supports_images: false,
                }],
            },
        );

        let models = configured_models(&config);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider, "local");
        assert_eq!(models[0].base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn preferred_write_target_returns_project_config_when_present() {
        let tempdir = tempdir().expect("tempdir");
        let project_root = tempdir.path().join("workspace");
        let nested = project_root.join("src/module");
        fs::create_dir_all(nested.join(".ignored")).expect("create nested dirs");
        fs::create_dir_all(project_root.join(".anie")).expect("create .anie dir");
        let project_config = project_root.join(".anie/config.toml");
        fs::write(&project_config, "[model]\nid = \"gpt-4o\"\n").expect("write config");

        assert_eq!(preferred_write_target(&nested), Some(project_config));
    }

    #[test]
    fn preferred_write_target_falls_back_to_global_config() {
        let tempdir = tempdir().expect("tempdir");
        let cwd = tempdir.path().join("workspace");
        fs::create_dir_all(&cwd).expect("create workspace");

        let global = global_config_path();
        assert_eq!(preferred_write_target(&cwd), global);
    }

    #[test]
    fn collect_context_files_respects_caps() {
        let tempdir = tempdir().expect("tempdir");
        let context_path = tempdir.path().join("AGENTS.md");
        fs::write(&context_path, "x".repeat(256)).expect("write context file");

        let files = collect_context_files(
            tempdir.path(),
            &ContextConfig {
                filenames: vec!["AGENTS.md".into()],
                max_file_bytes: 32,
                max_total_bytes: 32,
            },
        )
        .expect("collect context files");

        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert!(
            files[0]
                .contents
                .contains("[truncated due to context limits]")
        );
    }
}
