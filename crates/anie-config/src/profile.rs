//! Per-model capability profiles for local-coder reliability.
//!
//! Extends the existing `[model]` / `[[providers.*.models]]` config
//! rather than adding a parallel catalog. A selected model resolves
//! to one [`ResolvedModelProfile`] by merging, in order:
//!
//! 1. family inference from the model id (`qwen` → `qwen_coder`);
//! 2. matching `[[providers.<name>.models]]` entry;
//! 3. `[model]` section fields when the configured id matches;
//! 4. `[agent.local.tool_calls]` harness knobs.

use serde::{Deserialize, Serialize};

/// Prompt preset selected for a model.
///
/// `generic_local_coder`, `qwen_coder`, and `deepseek_coder` are
/// first-class. `llama_coder` / `mistral_coder` / `gemma_coder`
/// share the generic local-coder stance with a short family note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTemplateId {
    /// Neutral local-coder stance (evidence, inspect-first).
    GenericLocalCoder,
    /// Qwen Coder family. Stronger JSON/XML tool-call guidance.
    #[serde(alias = "qwen_coder_local")]
    QwenCoder,
    /// DeepSeek Coder family. Terse, one-action-at-a-time.
    DeepseekCoder,
    /// Llama coding variants. Stub over [`Self::GenericLocalCoder`].
    LlamaCoder,
    /// Mistral / Codestral variants. Stub over [`Self::GenericLocalCoder`].
    MistralCoder,
    /// Gemma coding variants. Stub over [`Self::GenericLocalCoder`].
    GemmaCoder,
}

impl PromptTemplateId {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GenericLocalCoder => "generic_local_coder",
            Self::QwenCoder => "qwen_coder",
            Self::DeepseekCoder => "deepseek_coder",
            Self::LlamaCoder => "llama_coder",
            Self::MistralCoder => "mistral_coder",
            Self::GemmaCoder => "gemma_coder",
        }
    }
}

/// How the model is expected to emit tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFormat {
    /// Provider-native `tool_calls` only. Hosted default.
    #[default]
    Native,
    /// `<tool_call>{"name","arguments"}</tool_call>` plus JSON fences.
    XmlJsonBlock,
    /// Markdown `anie-tool` / JSON fences only.
    JsonFence,
}

/// Optional per-model profile fields. Flattened onto `[model]` and
/// `[[providers.*.models]]` so a user writes the sprint TOML shape
/// without a nested table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelCapabilityProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<PromptTemplateId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_format: Option<ToolCallFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls_per_step: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_tool_per_step: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_parallel_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_repair: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub good_at: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub weak_at: Vec<String>,
}

impl ModelCapabilityProfile {
    fn is_empty(&self) -> bool {
        self.prompt_template.is_none()
            && self.tool_call_format.is_none()
            && self.preferred_temperature.is_none()
            && self.max_tool_calls_per_step.is_none()
            && self.one_tool_per_step.is_none()
            && self.supports_parallel_tools.is_none()
            && self.tool_call_repair.is_none()
            && self.good_at.is_empty()
            && self.weak_at.is_empty()
    }

    pub(crate) fn overlay(&mut self, other: &Self) {
        if other.prompt_template.is_some() {
            self.prompt_template = other.prompt_template;
        }
        if other.tool_call_format.is_some() {
            self.tool_call_format = other.tool_call_format;
        }
        if other.preferred_temperature.is_some() {
            self.preferred_temperature = other.preferred_temperature;
        }
        if other.max_tool_calls_per_step.is_some() {
            self.max_tool_calls_per_step = other.max_tool_calls_per_step;
        }
        if other.one_tool_per_step.is_some() {
            self.one_tool_per_step = other.one_tool_per_step;
        }
        if other.supports_parallel_tools.is_some() {
            self.supports_parallel_tools = other.supports_parallel_tools;
        }
        if other.tool_call_repair.is_some() {
            self.tool_call_repair = other.tool_call_repair;
        }
        if !other.good_at.is_empty() {
            self.good_at.clone_from(&other.good_at);
        }
        if !other.weak_at.is_empty() {
            self.weak_at.clone_from(&other.weak_at);
        }
    }
}

/// `[agent]` harness knobs. Only `local.tool_calls` is used in Phase B.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    #[serde(default)]
    pub local: LocalAgentConfig,
}

/// Local-model agent policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LocalAgentConfig {
    #[serde(default)]
    pub tool_calls: LocalToolCallConfig,
}

/// Bounded parse-repair policy for embedded tool calls.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalToolCallConfig {
    /// How many times the harness asks for a corrected tool call
    /// after a malformed parse. Default 2.
    #[serde(default = "default_max_parse_repairs")]
    pub max_parse_repairs: u32,
    /// When false (default), a parse that cannot uniquely name
    /// one call is not executed.
    #[serde(default)]
    pub execute_ambiguous_calls: bool,
}

impl Default for LocalToolCallConfig {
    fn default() -> Self {
        Self {
            max_parse_repairs: default_max_parse_repairs(),
            execute_ambiguous_calls: false,
        }
    }
}

pub const DEFAULT_MAX_PARSE_REPAIRS: u32 = 2;

fn default_max_parse_repairs() -> u32 {
    DEFAULT_MAX_PARSE_REPAIRS
}

/// Fully resolved profile consumed by the agent and prompt builder.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedModelProfile {
    pub prompt_template: Option<PromptTemplateId>,
    pub tool_call_format: ToolCallFormat,
    pub preferred_temperature: Option<f32>,
    pub max_tool_calls_per_step: Option<u32>,
    pub tool_call_repair: bool,
    pub good_at: Vec<String>,
    pub weak_at: Vec<String>,
    pub max_parse_repairs: u32,
    pub execute_ambiguous_calls: bool,
}

impl ResolvedModelProfile {
    /// Hosted / unset default: native tool calls, no local preset.
    #[must_use]
    pub fn hosted_default() -> Self {
        Self {
            prompt_template: None,
            tool_call_format: ToolCallFormat::Native,
            preferred_temperature: None,
            max_tool_calls_per_step: None,
            tool_call_repair: false,
            good_at: Vec::new(),
            weak_at: Vec::new(),
            max_parse_repairs: 0,
            execute_ambiguous_calls: false,
        }
    }

    /// Stable cache key so a model switch rebuilds the system prompt.
    #[must_use]
    pub fn cache_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.prompt_template
                .map(PromptTemplateId::as_str)
                .unwrap_or("none"),
            match self.tool_call_format {
                ToolCallFormat::Native => "native",
                ToolCallFormat::XmlJsonBlock => "xml_json_block",
                ToolCallFormat::JsonFence => "json_fence",
            },
            self.max_tool_calls_per_step.unwrap_or(0),
        )
    }

    #[must_use]
    pub fn uses_local_preset(&self) -> bool {
        self.prompt_template.is_some()
    }
}

/// Infer a prompt preset from a model id. Hosted frontier ids
/// (`gpt-*`, `claude-*`, `o1`/`o3`/`o4*`) return `None`.
#[must_use]
pub fn infer_prompt_template(model_id: &str) -> Option<PromptTemplateId> {
    let lowered = model_id.to_ascii_lowercase();
    let slug = lowered.rsplit('/').next().unwrap_or(&lowered);
    if looks_hosted_frontier(slug) {
        return None;
    }
    if slug.contains("qwen") {
        Some(PromptTemplateId::QwenCoder)
    } else if slug.contains("deepseek") {
        Some(PromptTemplateId::DeepseekCoder)
    } else if slug.contains("llama") {
        Some(PromptTemplateId::LlamaCoder)
    } else if slug.contains("mistral") || slug.contains("codestral") {
        Some(PromptTemplateId::MistralCoder)
    } else if slug.contains("gemma") {
        Some(PromptTemplateId::GemmaCoder)
    } else {
        None
    }
}

fn looks_hosted_frontier(slug: &str) -> bool {
    slug.starts_with("gpt-")
        || slug.starts_with("chatgpt")
        || slug.starts_with("claude")
        || slug.starts_with("o1")
        || slug.starts_with("o3")
        || slug.starts_with("o4")
        || slug.starts_with("gemini")
}

/// Infer a local-coder preset for an Ollama (or similarly local)
/// model whose id did not match a known family.
#[must_use]
pub fn infer_local_fallback(provider: &str, api_is_ollama: bool) -> Option<PromptTemplateId> {
    if api_is_ollama || provider.eq_ignore_ascii_case("ollama") {
        Some(PromptTemplateId::GenericLocalCoder)
    } else {
        None
    }
}

pub(crate) fn resolve_profile(
    model_section: &ModelCapabilityProfile,
    model_section_id: &str,
    catalog: Option<&ModelCapabilityProfile>,
    provider: &str,
    model_id: &str,
    api_is_ollama: bool,
    tool_calls: &LocalToolCallConfig,
) -> ResolvedModelProfile {
    let inferred = infer_prompt_template(model_id)
        .or_else(|| infer_local_fallback(provider, api_is_ollama));

    let mut merged = ModelCapabilityProfile {
        prompt_template: inferred,
        tool_call_format: inferred.map(|_| ToolCallFormat::XmlJsonBlock),
        preferred_temperature: inferred.map(|_| 0.1),
        max_tool_calls_per_step: inferred.map(|_| 1),
        one_tool_per_step: inferred.map(|_| true),
        supports_parallel_tools: inferred.map(|_| false),
        tool_call_repair: inferred.map(|_| true),
        good_at: Vec::new(),
        weak_at: Vec::new(),
    };

    if let Some(catalog) = catalog {
        merged.overlay(catalog);
    }
    if model_section_id == model_id && !model_section.is_empty() {
        merged.overlay(model_section);
    }

    let one_tool = merged.one_tool_per_step == Some(true)
        || merged.supports_parallel_tools == Some(false)
        || merged.max_tool_calls_per_step == Some(1);
    let max_tool_calls_per_step = if one_tool {
        Some(1)
    } else {
        merged.max_tool_calls_per_step
    };

    let uses_local = merged.prompt_template.is_some();
    ResolvedModelProfile {
        prompt_template: merged.prompt_template,
        tool_call_format: merged.tool_call_format.unwrap_or(ToolCallFormat::Native),
        preferred_temperature: merged.preferred_temperature,
        max_tool_calls_per_step,
        tool_call_repair: merged.tool_call_repair.unwrap_or(false),
        good_at: merged.good_at,
        weak_at: merged.weak_at,
        max_parse_repairs: if uses_local {
            tool_calls.max_parse_repairs
        } else {
            0
        },
        execute_ambiguous_calls: tool_calls.execute_ambiguous_calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_prompt_template_maps_known_local_families() {
        assert_eq!(
            infer_prompt_template("qwen2.5-coder:14b"),
            Some(PromptTemplateId::QwenCoder)
        );
        assert_eq!(
            infer_prompt_template("ollama/deepseek-coder-v2"),
            Some(PromptTemplateId::DeepseekCoder)
        );
        assert_eq!(
            infer_prompt_template("llama3.3:8b"),
            Some(PromptTemplateId::LlamaCoder)
        );
        assert_eq!(
            infer_prompt_template("codestral"),
            Some(PromptTemplateId::MistralCoder)
        );
        assert_eq!(
            infer_prompt_template("gemma4:e4b"),
            Some(PromptTemplateId::GemmaCoder)
        );
    }

    #[test]
    fn infer_prompt_template_leaves_hosted_frontier_unset() {
        assert_eq!(infer_prompt_template("gpt-4o"), None);
        assert_eq!(infer_prompt_template("claude-sonnet-4"), None);
        assert_eq!(infer_prompt_template("o4-mini"), None);
        assert_eq!(infer_prompt_template("gemini-2.0-flash"), None);
    }

    #[test]
    fn qwen_coder_local_alias_deserializes() {
        #[derive(Deserialize)]
        struct Wrap {
            id: PromptTemplateId,
        }
        let wrap: Wrap = toml::from_str("id = \"qwen_coder_local\"").expect("alias");
        assert_eq!(wrap.id, PromptTemplateId::QwenCoder);
    }
}
