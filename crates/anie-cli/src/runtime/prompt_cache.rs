//! Cached system prompt + file-modification stamp.
//!
//! Rebuilding the system prompt walks project-context files
//! (AGENTS.md, CLAUDE.md, etc.) from the current directory upward,
//! reads them, and concatenates them. That's cheap but not free.
//! The controller calls `refresh_if_stale` at the start of each
//! turn; this module owns the comparison logic.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use anie_agent::ToolRegistry;
use anie_config::AnieConfig;

use crate::skills::SkillRegistry;

/// Owns the latest system-prompt text plus the stamp of context
/// files it was built from. The stamp is a `Vec<(path, mtime)>`
/// rather than a single max-mtime so deletions of older files are
/// detected.
pub(crate) struct SystemPromptCache {
    system_prompt: String,
    context_files_stamp: Vec<(PathBuf, Option<SystemTime>)>,
}

impl SystemPromptCache {
    /// Build the cache fresh from the given context.
    pub(crate) fn build(
        cwd: &Path,
        tools: &ToolRegistry,
        skills: &SkillRegistry,
        config: &AnieConfig,
    ) -> Result<Self> {
        let system_prompt = crate::controller::build_system_prompt(cwd, tools, skills, config)?;
        let context_files_stamp = crate::controller::context_files_stamp(cwd, config);
        Ok(Self {
            system_prompt,
            context_files_stamp,
        })
    }

    /// Return the current cached system prompt.
    pub(crate) fn current(&self) -> &str {
        &self.system_prompt
    }

    /// Replace the cache wholesale (used by `reload_config`).
    pub(crate) fn replace(
        &mut self,
        cwd: &Path,
        tools: &ToolRegistry,
        skills: &SkillRegistry,
        config: &AnieConfig,
    ) -> Result<()> {
        *self = Self::build(cwd, tools, skills, config)?;
        Ok(())
    }

    /// Rebuild the prompt if the set of context files or any of
    /// their mtimes changed. Returns `true` if a rebuild happened.
    pub(crate) fn refresh_if_stale(
        &mut self,
        cwd: &Path,
        tools: &ToolRegistry,
        skills: &SkillRegistry,
        config: &AnieConfig,
    ) -> bool {
        let current_stamp = crate::controller::context_files_stamp(cwd, config);
        if current_stamp == self.context_files_stamp {
            return false;
        }
        let Ok(prompt) = crate::controller::build_system_prompt(cwd, tools, skills, config) else {
            // Rebuild failed — leave the cache as-is rather than
            // poisoning it with a partial value. The stamp stays
            // unchanged so we'll retry next turn.
            return false;
        };
        self.system_prompt = prompt;
        self.context_files_stamp = current_stamp;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, thread, time::Duration};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn context_files_stamp_detects_deleted_non_newest_file() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        let nested = root.join("src/module");
        fs::create_dir_all(&nested).expect("create nested dirs");

        let project_agents = root.join("AGENTS.md");
        let nested_claude = nested.join("CLAUDE.md");
        fs::write(&project_agents, "project context").expect("write agents");
        thread::sleep(Duration::from_millis(5));
        fs::write(&nested_claude, "nested context").expect("write claude");

        let config = AnieConfig::default();
        let first = crate::controller::context_files_stamp(&nested, &config);
        assert_eq!(first.len(), 2);

        fs::remove_file(&project_agents).expect("remove agents");
        let second = crate::controller::context_files_stamp(&nested, &config);

        assert_ne!(
            first, second,
            "deleting a non-newest file should change stamp"
        );
        assert_eq!(second.len(), 1);
    }
}
