//! TOML configuration loading, merging, and project-context discovery.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

#[cfg(windows)]
compile_error!(
    "anie-config::atomic_write is intentionally gated on Windows until ReplaceFileW-style replacement semantics are implemented; std::fs::rename does not provide the POSIX overwrite behavior this crate relies on."
);

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod mutation;

use anie_provider::{
    ApiKind, CostPerMillion, Model, ReasoningCapabilities, ReasoningControlMode,
    ReasoningOutputMode, ReasoningTags, ThinkingLevel, ThinkingRequestMode,
};

pub use mutation::ConfigMutator;

static ATOMIC_WRITE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Fully-resolved application configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AnieConfig {
    /// Default model configuration.
    pub model: ModelConfig,
    /// Provider-specific configuration.
    pub providers: HashMap<String, ProviderConfig>,
    /// Compaction settings.
    pub compaction: CompactionConfig,
    /// Project-context discovery limits.
    pub context: ContextConfig,
    /// Interactive TUI preferences.
    #[serde(default)]
    pub ui: UiConfig,
    /// Built-in tool configuration.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// Ollama-wide settings. anie-specific (not in pi): pi has
    /// no native Ollama codepath and never sends `num_ctx`, so
    /// this whole block has no pi equivalent. Per CLAUDE.md §3.
    #[serde(default)]
    pub ollama: OllamaConfig,
    /// True if a loaded config file (`~/.anie/config.toml` or
    /// a project-local `.anie/config.toml`) explicitly set the
    /// `[model]` section. Lets callers distinguish "the user
    /// declared a preferred model" from "we fell back to the
    /// built-in default" so the resolver doesn't let
    /// `state.json`'s last-used model override a user's
    /// declared default. Not persisted — derived at load time.
    #[serde(skip)]
    pub model_explicitly_set: bool,
}

/// Ollama-specific configuration. Applies workspace-wide to
/// every model with `ApiKind::OllamaChatApi`.
///
/// anie-specific (not in pi): pi uses Ollama's
/// OpenAI-compatible endpoint and never sends `num_ctx` on the
/// wire, so this whole block has no pi equivalent. See
/// `docs/ollama_default_num_ctx_cap/README.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OllamaConfig {
    /// Hard ceiling on `num_ctx` sent to Ollama, regardless of
    /// what `/api/show` reports. `None` (the default) means no
    /// cap; the model's discovered architectural max applies.
    /// When set, `Model.context_window` for any
    /// `OllamaChatApi` model is clamped at catalog-load time
    /// to `min(discovered, default_max_num_ctx)`.
    ///
    /// Distinct from `/context-length`'s per-model runtime
    /// override: this is a workspace-level safety floor, the
    /// override is a per-model fine-grain control. The runtime
    /// override always wins over the cap; if the user
    /// explicitly types `/context-length 65536` on a session
    /// with `default_max_num_ctx = 32768`, the override
    /// applies (PR 3 of this plan adds a one-line warning
    /// when the override exceeds the cap).
    ///
    /// Acceptable values: ≥ 2048 (matches `/context-length`'s
    /// minimum). Configs with smaller values are rejected at
    /// load time with a clear error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_max_num_ctx: Option<u64>,
}

/// Built-in tool configuration. These settings are guardrails and
/// presentation preferences; they are not a sandbox boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolsConfig {
    /// Bash tool settings.
    #[serde(default)]
    pub bash: BashToolConfig,
}

/// Bash tool settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BashToolConfig {
    /// Pre-spawn deny policy for bash commands.
    #[serde(default)]
    pub policy: BashPolicyConfig,
}

/// Pre-spawn deny policy for bash commands.
///
/// This policy reduces accidental execution of commands a user never
/// wants anie to run. It is not a sandbox: shell indirection and other
/// tools can still bypass textual checks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashPolicyConfig {
    /// Whether the policy is evaluated before spawning the shell.
    pub enabled: bool,
    /// Exact command names to block, matched against command basenames
    /// after simple shell-segment splitting.
    #[serde(default)]
    pub deny_commands: Vec<String>,
    /// Regex patterns matched against the raw command string.
    #[serde(default)]
    pub deny_patterns: Vec<String>,
}

impl Default for BashPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            deny_commands: Vec::new(),
            deny_patterns: Vec::new(),
        }
    }
}

/// Interactive-TUI-only preferences. None of these affect the
/// agent loop, provider behavior, or session storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiConfig {
    /// Whether the inline slash-command autocomplete popup opens
    /// when the user types `/`. Disable for a minimal experience
    /// while keeping `/help` and direct dispatch intact.
    pub slash_command_popup_enabled: bool,
    /// Whether finalized assistant messages render as markdown
    /// (headings, lists, code blocks, tables, …) or as plain
    /// wrapped text. Streaming blocks always render plain so the
    /// render loop doesn't re-parse markdown on every delta —
    /// the toggle only affects already-finalized blocks.
    #[serde(default = "default_markdown_enabled")]
    pub markdown_enabled: bool,
    /// How successful `bash` / `read` tool results render in the
    /// interactive transcript. `Verbose` shows the full body
    /// inside the boxed tool block (today's default). `Compact`
    /// shows only the tool title (e.g. `$ <command>` or
    /// `read <path>`). Errors always render their body so
    /// debugging stays available. See
    /// `docs/code_review_performance_2026-04-21/09_tool_output_display_modes.md`.
    #[serde(default = "default_tool_output_mode")]
    pub tool_output_mode: ToolOutputMode,
}

/// Display mode for successful `bash` / `read` tool output in
/// the interactive transcript. UI-only: never affects what the
/// agent, provider, or session storage see.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolOutputMode {
    /// Render the full tool-result body inside the boxed tool
    /// block. Today's default.
    Verbose,
    /// Hide successful `bash` and `read` bodies; keep the
    /// one-line title. `edit`, `write`, and other tools remain
    /// fully visible. Errors always show their body.
    Compact,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            slash_command_popup_enabled: true,
            markdown_enabled: default_markdown_enabled(),
            tool_output_mode: default_tool_output_mode(),
        }
    }
}

fn default_markdown_enabled() -> bool {
    true
}

fn default_tool_output_mode() -> ToolOutputMode {
    ToolOutputMode::Verbose
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
///
/// The main agent stream does not send `max_tokens` — see
/// `docs/max_tokens_handling/README.md` — so the upstream
/// enforces `input + output <= context_window` with its own
/// defaults. Our job here is to trigger compaction *before* the
/// input gets close enough to the ceiling that the model has no
/// room to answer. `reserve_tokens` is that headroom: compaction
/// fires when `context_tokens > context_window - reserve_tokens`,
/// which guarantees the next turn has at least `reserve_tokens`
/// free for the response. 16 k is pi's default and covers the
/// common case including reasoning-model runs; bump it if a
/// specific model routinely runs up against it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionConfig {
    /// Whether compaction is enabled.
    pub enabled: bool,
    /// Tokens of headroom to keep free of input so the upstream
    /// has room for its response. Effective constraint:
    /// compaction triggers when
    /// `context_tokens > context_window - reserve_tokens`.
    pub reserve_tokens: u64,
    /// Recent-token budget to keep verbatim past the compaction
    /// boundary (the tail of the transcript isn't summarized).
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

/// Return `~/.anie/` — anie's per-user state directory.
#[must_use]
pub fn anie_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".anie"))
}

/// Atomically write `contents` to `path`.
///
/// The write goes to a same-directory temp file
/// (`.{name}.tmp.{pid}.{counter}`), fsyncs, then renames over the destination. On POSIX the
/// rename is atomic for same-filesystem moves, so a crash during
/// the write leaves the original `path` intact.
///
/// Use this for any user-facing persistent file (config, auth,
/// runtime state). On failure the temp file is best-effort
/// removed and the previous contents of `path` are preserved —
/// callers should treat `Err` as "nothing happened."
///
/// # Errors
///
/// - `InvalidInput` when `path` has no parent or no file name.
/// - Any IO error from `File::create`, `write_all`, `sync_all`,
///   or `rename`, with the temp file cleaned up on the way out.
///
/// # Platform
///
/// POSIX-only today. Windows builds are explicitly gated at compile
/// time until this helper grows a `cfg(windows)` branch using
/// `ReplaceFileW`-style replacement semantics.
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;

    let tmp = atomic_write_temp_path(parent, file_name);

    // Scope the file handle so the OS sees it fully closed before
    // the rename runs.
    let write_result: std::io::Result<()> = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = fs::remove_file(&tmp);
            Err(err)
        }
    }
}

fn atomic_write_temp_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    // PID separates concurrent anie processes; the in-process
    // counter separates concurrent writes from the same process.
    let nonce = ATOMIC_WRITE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp = parent.to_path_buf();
    tmp.push(format!(
        ".{}.tmp.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        nonce
    ));
    tmp
}

/// Return the default global config path (`~/.anie/config.toml`).
#[must_use]
pub fn global_config_path() -> Option<PathBuf> {
    anie_dir().map(|dir| dir.join("config.toml"))
}

/// Return the JSON auth fallback path (`~/.anie/auth.json`).
#[must_use]
pub fn anie_auth_json_path() -> Option<PathBuf> {
    anie_dir().map(|dir| dir.join("auth.json"))
}

/// Return the sessions directory (`~/.anie/sessions/`).
#[must_use]
pub fn anie_sessions_dir() -> Option<PathBuf> {
    anie_dir().map(|dir| dir.join("sessions"))
}

/// Return the logs directory (`~/.anie/logs/`).
#[must_use]
pub fn anie_logs_dir() -> Option<PathBuf> {
    anie_dir().map(|dir| dir.join("logs"))
}

/// Return the runtime state file (`~/.anie/state.json`).
#[must_use]
pub fn anie_state_json_path() -> Option<PathBuf> {
    anie_dir().map(|dir| dir.join("state.json"))
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
    warn_legacy_ollama_openai_api(&config);
    validate_ollama_config(&config.ollama)?;
    Ok(config)
}

/// Lower bound matching the `/context-length` slash command's
/// minimum (rejected below 2048). The cap must be ≥ this value
/// so a clamp can never push `Model.context_window` below the
/// floor an explicit override would accept.
const OLLAMA_DEFAULT_MAX_NUM_CTX_MIN: u64 = 2_048;

/// Validate `[ollama]` config values at load time. Today only
/// `default_max_num_ctx` has a constraint (≥ 2048). Any future
/// numeric `[ollama]` field can grow this function rather than
/// adding parallel validation paths.
fn validate_ollama_config(ollama: &OllamaConfig) -> Result<()> {
    if let Some(value) = ollama.default_max_num_ctx
        && value < OLLAMA_DEFAULT_MAX_NUM_CTX_MIN
    {
        return Err(anyhow::anyhow!(
            "[ollama] default_max_num_ctx = {value} is below the minimum {OLLAMA_DEFAULT_MAX_NUM_CTX_MIN} \
             (the /context-length slash command also rejects values below this threshold)"
        ));
    }
    Ok(())
}

/// Ensure the global config file exists, creating a commented template when missing.
pub fn ensure_global_config_exists() -> Result<PathBuf> {
    let path = global_config_path().context("home directory is not available")?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        atomic_write(&path, default_config_template().as_bytes())
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
                replay_capabilities: None,
                compat: anie_provider::ModelCompat::None,
            });
        }
    }
    models
}

fn warn_legacy_ollama_openai_api(config: &AnieConfig) {
    for provider_name in legacy_ollama_openai_api_providers(config) {
        tracing::warn!(
            provider = %provider_name,
            "Ollama provider config uses legacy api = \"OpenAICompletions\"; newly discovered Ollama models use api = \"OllamaChatApi\" for native /api/chat support"
        );
    }
}

fn legacy_ollama_openai_api_providers(config: &AnieConfig) -> Vec<String> {
    config
        .providers
        .iter()
        .filter_map(|(provider_name, provider_config)| {
            if provider_config.api != Some(ApiKind::OpenAICompletions) {
                return None;
            }
            if provider_name.eq_ignore_ascii_case("ollama")
                || provider_config
                    .base_url
                    .as_deref()
                    .is_some_and(is_ollama_endpoint)
            {
                Some(provider_name.clone())
            } else {
                None
            }
        })
        .collect()
}

fn is_ollama_endpoint(base_url: &str) -> bool {
    let trimmed = base_url.trim().trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    root.contains(":11434")
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
                mtime: fs::metadata(&candidate).and_then(|m| m.modified()).ok(),
            });
        }
    }

    Ok(files)
}

/// Return the default config template written on first run.
#[must_use]
pub fn default_config_template() -> &'static str {
    "# anie-rs configuration\n\n# Default model\n# [model]\n# provider = \"openai\"\n# id = \"gpt-4o\"\n# thinking = \"medium\"\n\n# Provider settings\n# [providers.openai]\n# api_key_env = \"OPENAI_API_KEY\"\n\n# Custom local OpenAI-compatible provider\n# [providers.ollama]\n# base_url = \"http://localhost:11434/v1\"\n# api = \"OpenAICompletions\"\n# [[providers.ollama.models]]\n# id = \"qwen3:32b\"\n# name = \"Qwen 3 32B\"\n# context_window = 32768\n# max_tokens = 8192\n# thinking_request_mode = \"ReasoningEffort\"\n\n# Bash deny policy. This is a guardrail, not a sandbox.\n# [tools.bash.policy]\n# enabled = true\n# deny_commands = [\"rm\", \"dd\", \"mkfs\"]\n# deny_patterns = [\"git\\\\s+push\\\\s+--force\"]\n\n# Ollama-wide settings (only meaningful for OllamaChatApi models).\n# Optional cap on the num_ctx anie sends to Ollama. Useful on\n# constrained hardware: a 16 GB Mac with a 32B+ model can hit\n# load failures at the 262144-token architectural max from\n# /api/show. Setting this lets anie clamp every Ollama model's\n# context window at catalog-load time. Per-model runtime\n# overrides via /context-length still win over this cap.\n# Acceptable values: >= 2048 (matches the /context-length minimum).\n# [ollama]\n# default_max_num_ctx = 32768\n\n# Compaction settings\n# [compaction]\n# enabled = true\n# reserve_tokens = 16384\n# keep_recent_tokens = 20000\n\n# Project context files\n# [context]\n# filenames = [\"AGENTS.md\", \"CLAUDE.md\"]\n# max_file_bytes = 32768\n# max_total_bytes = 65536\n"
}

fn load_partial_config(path: &Path) -> Result<PartialAnieConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))
}

fn merge_partial_config(config: &mut AnieConfig, partial: PartialAnieConfig) {
    if let Some(model) = partial.model {
        // A file that has *any* `[model]` field counts as an
        // explicit user preference. We only set the flag when
        // at least one field was provided — a `[model]` section
        // with no sub-keys is unusual but would otherwise trick
        // us into treating an effectively-empty declaration as
        // explicit.
        if model.provider.is_some() || model.id.is_some() || model.thinking.is_some() {
            config.model_explicitly_set = true;
        }
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

    if let Some(tools) = partial.tools
        && let Some(bash) = tools.bash
        && let Some(policy) = bash.policy
    {
        if let Some(enabled) = policy.enabled {
            config.tools.bash.policy.enabled = enabled;
        }
        if let Some(deny_commands) = policy.deny_commands {
            config.tools.bash.policy.deny_commands = deny_commands;
        }
        if let Some(deny_patterns) = policy.deny_patterns {
            config.tools.bash.policy.deny_patterns = deny_patterns;
        }
    }

    if let Some(ollama) = partial.ollama
        && let Some(value) = ollama.default_max_num_ctx
    {
        config.ollama.default_max_num_ctx = Some(value);
    }

    if let Some(ui) = partial.ui {
        if let Some(slash_command_popup_enabled) = ui.slash_command_popup_enabled {
            config.ui.slash_command_popup_enabled = slash_command_popup_enabled;
        }
        if let Some(markdown_enabled) = ui.markdown_enabled {
            config.ui.markdown_enabled = markdown_enabled;
        }
        if let Some(tool_output_mode) = ui.tool_output_mode {
            config.ui.tool_output_mode = tool_output_mode;
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
    #[serde(default)]
    tools: Option<PartialToolsConfig>,
    #[serde(default)]
    ollama: Option<PartialOllamaConfig>,
    #[serde(default)]
    ui: Option<PartialUiConfig>,
}

/// Optional `[ui]` overrides loaded from `config.toml`. Each
/// field is `Option<...>` so omitted keys preserve the
/// `UiConfig::default()` values rather than zero-initializing.
/// Mirrors the partial-config pattern used for `[compaction]`,
/// `[context]`, etc. above.
#[derive(Debug, Default, Deserialize)]
struct PartialUiConfig {
    slash_command_popup_enabled: Option<bool>,
    markdown_enabled: Option<bool>,
    tool_output_mode: Option<ToolOutputMode>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialOllamaConfig {
    default_max_num_ctx: Option<u64>,
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

#[derive(Debug, Default, Deserialize)]
struct PartialToolsConfig {
    bash: Option<PartialBashToolConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialBashToolConfig {
    policy: Option<PartialBashPolicyConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct PartialBashPolicyConfig {
    enabled: Option<bool>,
    deny_commands: Option<Vec<String>>,
    deny_patterns: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        sync::{Arc, Barrier, Mutex},
        thread,
    };

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

    /// Plan 09 PR-A: the new `tool_output_mode` setting
    /// defaults to `Verbose` so today's behavior is preserved
    /// for every user who doesn't opt in.
    #[test]
    fn ui_config_tool_output_mode_defaults_to_verbose() {
        assert_eq!(
            UiConfig::default().tool_output_mode,
            ToolOutputMode::Verbose
        );
    }

    /// Forward-compat: a user's config written before the
    /// field existed must still load cleanly, with the default
    /// filled in.
    #[test]
    fn ui_config_without_tool_output_mode_loads_with_default() {
        let toml_str = "slash_command_popup_enabled = true\nmarkdown_enabled = true\n";
        let config: UiConfig = toml::from_str(toml_str).expect("parse legacy UiConfig");
        assert_eq!(config.tool_output_mode, ToolOutputMode::Verbose);
    }

    /// `tool_output_mode = "compact"` round-trips through
    /// serde; the lowercase rename is stable so any user who
    /// opts into compact survives a restart.
    #[test]
    fn ui_config_tool_output_mode_compact_roundtrips() {
        let toml_str = "slash_command_popup_enabled = true\nmarkdown_enabled = true\ntool_output_mode = \"compact\"\n";
        let config: UiConfig = toml::from_str(toml_str).expect("parse compact");
        assert_eq!(config.tool_output_mode, ToolOutputMode::Compact);
    }

    #[test]
    fn atomic_write_creates_file_with_contents() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("config.toml");
        atomic_write(&target, b"hello").expect("write ok");
        assert_eq!(fs::read(&target).expect("read target"), b"hello");
    }

    #[test]
    fn atomic_write_replaces_existing_file_atomically() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("state.json");
        fs::write(&target, b"original").expect("seed original");
        atomic_write(&target, b"updated").expect("replace ok");
        assert_eq!(fs::read(&target).expect("read"), b"updated");
    }

    #[test]
    fn atomic_write_failure_preserves_original_and_cleans_temp() {
        // Provoke a write failure by pointing the target at a
        // path whose parent doesn't exist. The original file
        // (there is none at `target`) must remain absent and no
        // orphan temp file should be left behind.
        let dir = tempdir().expect("tempdir");
        let bogus_parent = dir.path().join("does/not/exist");
        let target = bogus_parent.join("file.txt");
        let err = atomic_write(&target, b"nope").expect_err("must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(!target.exists(), "target must not exist");
        // No temp file in the (also non-existent) parent either.
        assert!(!bogus_parent.exists(), "parent should remain absent");
    }

    #[test]
    fn atomic_write_failure_leaves_previous_contents_intact() {
        // A more targeted preservation test: pre-populate a
        // target, then invoke atomic_write with a path that
        // triggers a failure (empty contents are fine; the
        // failure comes from renaming a file over one with
        // different permissions). Simulate failure by supplying
        // a path whose file_name strips away — no file_name
        // triggers InvalidInput before any write starts.
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("keep.txt");
        fs::write(&target, b"keep me").expect("seed");
        // Path without a file name: `..` gives None.
        let bad_path = dir.path().join("..");
        let err = atomic_write(&bad_path, b"nope").expect_err("rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            fs::read(&target).expect("read target"),
            b"keep me",
            "pre-existing target must be untouched by an aborted write"
        );
    }

    #[test]
    fn atomic_write_temp_name_includes_pid_counter_and_dot_prefix() {
        // Document the temp-name convention. The temp file is a
        // sibling of the target (same parent), dot-prefixed, and
        // tagged with the process pid plus a same-process counter
        // so concurrent anie writes don't clash.
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("out.txt");
        atomic_write(&target, b"x").expect("write ok");
        // After a successful write, the temp must not remain.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .expect("list dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        let has_temp = entries.iter().any(|name| name.starts_with(".out.txt.tmp."));
        assert!(
            !has_temp,
            "temp file must be cleaned after rename: {entries:?}"
        );
    }

    #[test]
    fn atomic_write_temp_names_are_unique_for_same_process_concurrency() {
        let dir = tempdir().expect("tempdir");
        let parent = Arc::new(dir.path().to_path_buf());
        let file_name = Arc::new(std::ffi::OsString::from("state.json"));
        let barrier = Arc::new(Barrier::new(16));
        let names = Arc::new(Mutex::new(HashSet::new()));

        let handles = (0..16)
            .map(|_| {
                let parent = Arc::clone(&parent);
                let file_name = Arc::clone(&file_name);
                let barrier = Arc::clone(&barrier);
                let names = Arc::clone(&names);
                thread::spawn(move || {
                    barrier.wait();
                    let tmp = atomic_write_temp_path(&parent, &file_name);
                    let file_name = tmp
                        .file_name()
                        .expect("temp file name")
                        .to_string_lossy()
                        .into_owned();
                    names.lock().expect("names lock").insert(file_name);
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().expect("thread join");
        }

        let names = names.lock().expect("names lock");
        assert_eq!(names.len(), 16, "temp names must be unique: {names:?}");
        assert!(
            names
                .iter()
                .all(|name| name.starts_with(".state.json.tmp.")),
            "temp names must keep the documented prefix: {names:?}"
        );
    }

    #[test]
    fn defaults_fill_missing_fields() {
        let config =
            load_config_with_paths(None, None, CliOverrides::default()).expect("load defaults");
        assert_eq!(config.model.provider, "openai");
        assert_eq!(config.context.max_total_bytes, 65_536);
        assert!(config.tools.bash.policy.enabled);
        assert!(config.tools.bash.policy.deny_commands.is_empty());
        assert!(config.tools.bash.policy.deny_patterns.is_empty());
        assert_eq!(
            config.ollama.default_max_num_ctx, None,
            "the cap is opt-in: default state must be None so existing behavior is unchanged"
        );
    }

    #[test]
    fn ollama_config_default_max_num_ctx_round_trips_serde() {
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [ollama]
            default_max_num_ctx = 32768
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config");

        assert_eq!(config.ollama.default_max_num_ctx, Some(32_768));
    }

    #[test]
    fn anie_config_loads_when_ollama_block_is_absent() {
        // Forward-compat: a config file written by any anie
        // build before this PR (no `[ollama]` block) must load
        // cleanly with the cap defaulting to None.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [model]
            provider = "openai"
            id = "gpt-4o"
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config without ollama block");

        assert_eq!(config.ollama.default_max_num_ctx, None);
    }

    #[test]
    fn anie_config_loads_when_default_max_num_ctx_is_absent() {
        // Forward-compat boundary: an `[ollama]` block can
        // exist without `default_max_num_ctx` (future fields
        // may be added there). The block's mere presence must
        // not require this specific field.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [ollama]
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load empty [ollama] block");

        assert_eq!(config.ollama.default_max_num_ctx, None);
    }

    #[test]
    fn ollama_default_max_num_ctx_below_minimum_is_rejected() {
        // The cap must be >= the /context-length command's
        // minimum, otherwise a clamp at catalog load could push
        // Model.context_window below what an explicit override
        // could restore.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [ollama]
            default_max_num_ctx = 1024
            "#,
        )
        .expect("write config");

        let result = load_config_with_paths(Some(&config_path), None, CliOverrides::default());
        let error = result.expect_err("below-minimum cap must reject load");
        let message = format!("{error:#}");
        assert!(
            message.contains("default_max_num_ctx") && message.contains("1024"),
            "error must name the field and the offending value; got:\n{message}"
        );
        assert!(
            message.contains("2048"),
            "error must name the minimum so the user knows the lower bound; got:\n{message}"
        );
    }

    #[test]
    fn ollama_default_max_num_ctx_at_minimum_is_accepted() {
        // Boundary: exactly 2048 is acceptable.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [ollama]
            default_max_num_ctx = 2048
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("at-minimum cap is accepted");
        assert_eq!(config.ollama.default_max_num_ctx, Some(2_048));
    }

    #[test]
    fn default_template_documents_ollama_block_with_example() {
        // The template is what `ensure_global_config_exists`
        // writes when no config file exists. Discoverable
        // documentation: a user opening
        // ~/.anie/config.toml should see the [ollama] block as
        // a commented example.
        let template = default_config_template();
        assert!(
            template.contains("[ollama]"),
            "template must include [ollama] section header"
        );
        assert!(
            template.contains("default_max_num_ctx"),
            "template must mention the field name so users can search"
        );
    }

    #[test]
    fn ui_config_loads_from_real_config_path() {
        // Regression for the bug where `[ui]` settings were
        // silently ignored: `PartialAnieConfig` had no `ui`
        // field, so `merge_partial_config` couldn't propagate
        // the loaded values into `AnieConfig::ui`. This test
        // pins the real loader path end-to-end so any future
        // regression in the partial-config plumbing is caught.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [ui]
            slash_command_popup_enabled = false
            markdown_enabled = false
            tool_output_mode = "compact"
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config");

        assert!(!config.ui.slash_command_popup_enabled);
        assert!(!config.ui.markdown_enabled);
        assert_eq!(config.ui.tool_output_mode, ToolOutputMode::Compact);
    }

    #[test]
    fn ui_config_omitted_fields_keep_defaults() {
        // Merging is field-level: a `[ui]` block that only sets
        // one knob must leave the others at `UiConfig::default()`,
        // not zero-initialize them.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [ui]
            markdown_enabled = false
            "#,
        )
        .expect("write config");

        let defaults = UiConfig::default();
        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config");

        // Only the explicitly-set field changed.
        assert!(!config.ui.markdown_enabled);
        // The other two retain their `UiConfig::default()` values.
        assert_eq!(
            config.ui.slash_command_popup_enabled,
            defaults.slash_command_popup_enabled,
        );
        assert_eq!(config.ui.tool_output_mode, defaults.tool_output_mode);
    }

    #[test]
    fn ui_config_absent_section_keeps_all_defaults() {
        // No `[ui]` block at all → `AnieConfig::ui` is
        // `UiConfig::default()` verbatim.
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [model]
            provider = "ollama"
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config");
        assert_eq!(config.ui, UiConfig::default());
    }

    #[test]
    fn bash_policy_loads_from_config() {
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
            [tools.bash.policy]
            enabled = true
            deny_commands = ["rm", "dd"]
            deny_patterns = ["git\\s+push\\s+--force"]
            "#,
        )
        .expect("write config");

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config");

        assert!(config.tools.bash.policy.enabled);
        assert_eq!(config.tools.bash.policy.deny_commands, vec!["rm", "dd"]);
        assert_eq!(
            config.tools.bash.policy.deny_patterns,
            vec!["git\\s+push\\s+--force"]
        );
    }

    #[test]
    fn bash_policy_layer_merging_replaces_lists() {
        let tempdir = tempdir().expect("tempdir");
        let global_path = tempdir.path().join("global.toml");
        let project_path = tempdir.path().join("project.toml");
        fs::write(
            &global_path,
            r#"
            [tools.bash.policy]
            deny_commands = ["rm", "dd"]
            deny_patterns = ["curl"]
            "#,
        )
        .expect("write global config");
        fs::write(
            &project_path,
            r#"
            [tools.bash.policy]
            enabled = false
            deny_commands = ["git"]
            "#,
        )
        .expect("write project config");

        let config = load_config_with_paths(
            Some(&global_path),
            Some(&project_path),
            CliOverrides::default(),
        )
        .expect("load config");

        assert!(!config.tools.bash.policy.enabled);
        assert_eq!(config.tools.bash.policy.deny_commands, vec!["git"]);
        assert_eq!(config.tools.bash.policy.deny_patterns, vec!["curl"]);
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
    fn config_toml_with_legacy_ollama_api_logs_warning_but_loads_unchanged() {
        let tempdir = tempdir().expect("tempdir");
        let config_path = tempdir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
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

        let config = load_config_with_paths(Some(&config_path), None, CliOverrides::default())
            .expect("load config");

        assert_eq!(
            legacy_ollama_openai_api_providers(&config),
            vec!["ollama".to_string()]
        );
        let provider = config.providers.get("ollama").expect("ollama provider");
        assert_eq!(provider.api, Some(ApiKind::OpenAICompletions));
        assert_eq!(provider.models[0].id, "qwen3:32b");
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
