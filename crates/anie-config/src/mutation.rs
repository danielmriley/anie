use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use anie_provider::{ApiKind, Model};

use crate::default_config_template;

/// Comment-preserving config editor for `~/.anie/config.toml`.
pub struct ConfigMutator {
    doc: DocumentMut,
    path: PathBuf,
}

impl ConfigMutator {
    /// Load an existing config or create a document from the default template.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let contents = if path.is_file() {
            fs::read_to_string(path)
                .with_context(|| format!("failed to read config file {}", path.display()))?
        } else {
            default_config_template().to_string()
        };
        let doc = contents
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        Ok(Self {
            doc,
            path: path.to_path_buf(),
        })
    }

    /// Load an existing config document.
    pub fn load(path: &Path) -> Result<Self> {
        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let doc = contents
            .parse::<DocumentMut>()
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        Ok(Self {
            doc,
            path: path.to_path_buf(),
        })
    }

    /// Return the rendered TOML document.
    #[must_use]
    pub fn as_toml(&self) -> String {
        self.doc.to_string()
    }

    /// Set the default provider and model.
    pub fn set_default_model(&mut self, provider: &str, model_id: &str) {
        ensure_table(&mut self.doc["model"]);
        self.doc["model"]["provider"] = value(provider);
        self.doc["model"]["id"] = value(model_id);
    }

    /// Ensure a provider table exists.
    pub fn ensure_provider(&mut self, name: &str) {
        ensure_table(&mut self.doc["providers"]);
        ensure_table(&mut self.doc["providers"][name]);
    }

    /// Ensure a provider table exists, optionally updating the base URL and API kind.
    pub fn upsert_provider(&mut self, name: &str, base_url: Option<&str>, api: Option<ApiKind>) {
        self.ensure_provider(name);

        if let Some(base_url) = base_url {
            self.doc["providers"][name]["base_url"] = value(base_url);
        } else {
            remove_key_from_table(&mut self.doc["providers"][name], "base_url");
        }

        if let Some(api) = api {
            self.doc["providers"][name]["api"] = value(format!("{api:?}"));
        } else {
            remove_key_from_table(&mut self.doc["providers"][name], "api");
        }
    }

    /// Add or replace a custom model entry under the provider's `models` array.
    pub fn upsert_provider_model(&mut self, provider_name: &str, model: &Model) {
        ensure_table(&mut self.doc["providers"]);
        ensure_table(&mut self.doc["providers"][provider_name]);

        let models = ensure_models_array(&mut self.doc["providers"][provider_name]);
        if let Some(existing) = models.iter_mut().find(|table| {
            table
                .get("id")
                .and_then(Item::as_value)
                .and_then(|value| value.as_str())
                == Some(model.id.as_str())
        }) {
            populate_model_table(existing, model);
            return;
        }

        let mut table = Table::new();
        populate_model_table(&mut table, model);
        models.push(table);
    }

    /// Remove a provider from the config.
    pub fn remove_provider(&mut self, name: &str) {
        if let Some(providers) = self.doc.get_mut("providers")
            && let Some(table) = providers.as_table_mut()
        {
            table.remove(name);
        }
    }

    /// Persist the modified document back to disk.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        crate::atomic_write(&self.path, self.doc.to_string().as_bytes())
            .with_context(|| format!("failed to write config file {}", self.path.display()))
    }
}

fn ensure_table(item: &mut Item) {
    if !item.is_table() {
        *item = Item::Table(Table::new());
    }
}

fn remove_key_from_table(item: &mut Item, key: &str) {
    if let Some(table) = item.as_table_mut() {
        table.remove(key);
    }
}

// Internal helper: callers always pass an item created as a table, and we
// assign `ArrayOfTables` immediately before the second downcast. The
// invariants are local to this file.
#[allow(clippy::expect_used)]
fn ensure_models_array(provider_item: &mut Item) -> &mut ArrayOfTables {
    let table = provider_item
        .as_table_mut()
        .expect("provider item should be a table");
    let needs_models_array = table
        .get("models")
        .is_none_or(|item| !item.is_array_of_tables());
    if needs_models_array {
        table["models"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    table["models"]
        .as_array_of_tables_mut()
        .expect("models should be an array of tables")
}

fn populate_model_table(table: &mut Table, model: &Model) {
    table["id"] = value(model.id.as_str());
    table["name"] = value(model.name.as_str());
    table["context_window"] = value(i64::try_from(model.context_window).unwrap_or(i64::MAX));
    table["max_tokens"] = value(i64::try_from(model.max_tokens).unwrap_or(i64::MAX));
    table["supports_reasoning"] = value(model.supports_reasoning);
    table["supports_images"] = value(model.supports_images);

    if let Some(reasoning) = &model.reasoning_capabilities {
        if let Some(control) = reasoning.control {
            table["reasoning_control"] = value(format!("{control:?}"));
        } else {
            table.remove("reasoning_control");
        }
        if let Some(output) = reasoning.output {
            table["reasoning_output"] = value(format!("{output:?}"));
        } else {
            table.remove("reasoning_output");
        }
        if let Some(tags) = &reasoning.tags {
            table["reasoning_tag_open"] = value(tags.open.as_str());
            table["reasoning_tag_close"] = value(tags.close.as_str());
        } else {
            table.remove("reasoning_tag_open");
            table.remove("reasoning_tag_close");
        }
        if let Some(request_mode) = reasoning.request_mode {
            table["thinking_request_mode"] = value(format!("{request_mode:?}"));
        } else {
            table.remove("thinking_request_mode");
        }
    } else {
        table.remove("reasoning_control");
        table.remove("reasoning_output");
        table.remove("reasoning_tag_open");
        table.remove("reasoning_tag_close");
        table.remove("thinking_request_mode");
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use anie_provider::{
        CostPerMillion, ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode,
        ReasoningTags, ThinkingRequestMode,
    };

    fn sample_model() -> Model {
        Model {
            id: "qwen3:32b".into(),
            name: "Qwen 3 32B".into(),
            provider: "ollama".into(),
            api: ApiKind::OpenAICompletions,
            base_url: "http://localhost:11434/v1".into(),
            context_window: 32_768,
            max_tokens: 8_192,
            supports_reasoning: true,
            reasoning_capabilities: Some(ReasoningCapabilities {
                control: Some(ReasoningControlMode::Native),
                output: Some(ReasoningOutputMode::Tagged),
                tags: Some(ReasoningTags {
                    open: "<think>".into(),
                    close: "</think>".into(),
                }),
                request_mode: Some(ThinkingRequestMode::ReasoningEffort),
            }),
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
        }
    }

    #[test]
    fn upserting_provider_preserves_existing_comments() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# top comment\n\n[context]\n# context comment\nmax_total_bytes = 1024\n",
        )
        .expect("write config");

        let mut mutator = ConfigMutator::load(&path).expect("load mutator");
        mutator.upsert_provider(
            "ollama",
            Some("http://localhost:11434/v1"),
            Some(ApiKind::OpenAICompletions),
        );
        mutator.set_default_model("ollama", "qwen3:32b");
        let rendered = mutator.as_toml();

        assert!(rendered.contains("# top comment"));
        assert!(rendered.contains("# context comment"));
        assert!(rendered.contains("[providers.ollama]"));
        assert!(rendered.contains("base_url = \"http://localhost:11434/v1\""));
        assert!(rendered.contains("provider = \"ollama\""));
        assert!(rendered.contains("id = \"qwen3:32b\""));
    }

    #[test]
    fn upserting_provider_model_writes_reasoning_metadata() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut mutator = ConfigMutator::load_or_create(&path).expect("load mutator");

        mutator.upsert_provider(
            "ollama",
            Some("http://localhost:11434/v1"),
            Some(ApiKind::OpenAICompletions),
        );
        mutator.upsert_provider_model("ollama", &sample_model());
        let rendered = mutator.as_toml();

        assert!(rendered.contains("[[providers.ollama.models]]"));
        assert!(rendered.contains("reasoning_control = \"Native\""));
        assert!(rendered.contains("reasoning_output = \"Tagged\""));
        assert!(rendered.contains("reasoning_tag_open = \"<think>\""));
        assert!(rendered.contains("reasoning_tag_close = \"</think>\""));
        assert!(rendered.contains("thinking_request_mode = \"ReasoningEffort\""));
    }

    #[test]
    fn removing_provider_leaves_other_sections_intact() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[providers.ollama]\nbase_url = \"http://localhost:11434/v1\"\n\n[context]\nmax_total_bytes = 1024\n",
        )
        .expect("write config");

        let mut mutator = ConfigMutator::load(&path).expect("load mutator");
        mutator.remove_provider("ollama");
        let rendered = mutator.as_toml();

        assert!(!rendered.contains("[providers.ollama]"));
        assert!(rendered.contains("[context]"));
        assert!(rendered.contains("max_total_bytes = 1024"));
    }
}
