//! Slash-command registry with pi-style source tagging.
//!
//! Builtin commands, plus future extension / prompt-template /
//! skill-registered commands, all live in one `CommandRegistry`
//! so callers like `/help` can render them grouped by source.
//!
//! **Dispatch is NOT owned by this module.** The actual handling of
//! a slash command still happens in the controller's `handle_action`
//! match on `UiAction`. This module is pure metadata: name,
//! description, and origin. Mirrors pi-mono's `slash-commands.ts`
//! which takes the same approach.
//!
//! When extensions (plan 10) arrive, they register their commands
//! into this registry with `SlashCommandSource::Extension` and
//! dispatch through the extension host — not through `UiAction`.

use std::path::PathBuf;

/// Where a registered slash command came from.
///
/// Mirrors pi-mono's `SlashCommandSource` with an added `Builtin`
/// variant so every command has a uniform source tag.
///
/// Non-`Builtin` variants are unused today — they'll be constructed
/// once extensions (plan 10) and prompt/skill loaders land. Kept in
/// the type now so the registry API is stable.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SlashCommandSource {
    /// Shipped with anie; dispatched via `UiAction`.
    Builtin,
    /// Registered by an extension (plan 10).
    Extension { extension_name: String },
    /// Registered by a prompt template (tracked in `docs/ideas.md`).
    Prompt { template_path: PathBuf },
    /// Registered by a skill (tracked in `docs/ideas.md`).
    Skill { skill_name: String },
}

impl SlashCommandSource {
    /// Short human-readable origin label.
    #[allow(dead_code)] // used once /help lands
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Builtin => "builtin".to_string(),
            Self::Extension { extension_name } => format!("extension: {extension_name}"),
            Self::Prompt { template_path } => format!("prompt: {}", template_path.display()),
            Self::Skill { skill_name } => format!("skill: {skill_name}"),
        }
    }
}

/// Metadata for one registered slash command.
///
/// `summary` isn't read anywhere today; it'll feed `/help` once
/// that command lands. Kept in the struct now so the registry API
/// is stable.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct SlashCommandInfo {
    pub(crate) name: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) source: SlashCommandSource,
}

impl SlashCommandInfo {
    pub(crate) const fn builtin(name: &'static str, summary: &'static str) -> Self {
        Self {
            name,
            summary,
            source: SlashCommandSource::Builtin,
        }
    }
}

/// All slash commands known to this anie process.
///
/// Populated at startup with `builtin_commands()`; future extension
/// systems will call `register` to add their own entries.
pub(crate) struct CommandRegistry {
    commands: Vec<SlashCommandInfo>,
}

impl CommandRegistry {
    /// Create a registry pre-populated with the builtin command set.
    pub(crate) fn with_builtins() -> Self {
        Self {
            commands: builtin_commands(),
        }
    }

    /// Register a command. Duplicates (by name) are rejected — the
    /// first registration wins, matching pi's behavior.
    #[allow(dead_code)] // used by future extension / prompt / skill registration
    pub(crate) fn register(&mut self, command: SlashCommandInfo) -> Result<(), DuplicateCommand> {
        if self.commands.iter().any(|c| c.name == command.name) {
            return Err(DuplicateCommand {
                name: command.name.to_string(),
            });
        }
        self.commands.push(command);
        Ok(())
    }

    /// Look up a command by exact name.
    #[allow(dead_code)] // used once /help lands
    pub(crate) fn lookup(&self, name: &str) -> Option<&SlashCommandInfo> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// All registered commands, in registration order.
    #[allow(dead_code)] // used once /help lands
    pub(crate) fn all(&self) -> &[SlashCommandInfo] {
        &self.commands
    }

    /// Group registered commands by source. Order within each group
    /// is the original registration order.
    #[allow(dead_code)] // used once /help lands
    pub(crate) fn grouped_by_source(&self) -> Vec<(SourceKey, Vec<&SlashCommandInfo>)> {
        let mut groups: Vec<(SourceKey, Vec<&SlashCommandInfo>)> = Vec::new();
        for command in &self.commands {
            let key = SourceKey::from(&command.source);
            if let Some((_, entries)) = groups.iter_mut().find(|(k, _)| k == &key) {
                entries.push(command);
            } else {
                groups.push((key, vec![command]));
            }
        }
        groups
    }
}

/// Coarse grouping key for `grouped_by_source` — preserves source
/// kind but drops per-origin fields so all builtins group together,
/// all extensions group together, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // used once /help lands
pub(crate) enum SourceKey {
    Builtin,
    Extension,
    Prompt,
    Skill,
}

impl From<&SlashCommandSource> for SourceKey {
    fn from(source: &SlashCommandSource) -> Self {
        match source {
            SlashCommandSource::Builtin => Self::Builtin,
            SlashCommandSource::Extension { .. } => Self::Extension,
            SlashCommandSource::Prompt { .. } => Self::Prompt,
            SlashCommandSource::Skill { .. } => Self::Skill,
        }
    }
}

/// Error returned when attempting to register a command whose name
/// is already taken.
#[derive(Debug)]
#[allow(dead_code)] // used by future extension registration
pub(crate) struct DuplicateCommand {
    pub name: String,
}

/// The builtin anie slash-command catalog.
///
/// Keep this in sync with the `UiAction` dispatch in
/// `controller::InteractiveController::handle_action`. Adding a new
/// builtin means:
///   1. Adding a `UiAction` variant in `anie-tui::app`.
///   2. Handling it in `handle_action`.
///   3. Adding a new entry here so `/help` picks it up.
fn builtin_commands() -> Vec<SlashCommandInfo> {
    vec![
        SlashCommandInfo::builtin("model", "Select model (opens picker on no args)"),
        SlashCommandInfo::builtin("thinking", "Set reasoning effort: off|low|medium|high"),
        SlashCommandInfo::builtin("compact", "Manually compact the session context"),
        SlashCommandInfo::builtin("fork", "Create a child session branched from now"),
        SlashCommandInfo::builtin("diff", "Show file changes made in this session"),
        SlashCommandInfo::builtin("new", "Start a fresh session"),
        SlashCommandInfo::builtin(
            "session",
            "Show session info (or `/session list`, `/session <id>`)",
        ),
        SlashCommandInfo::builtin("tools", "List active tools"),
        SlashCommandInfo::builtin("onboard", "Reopen the onboarding flow"),
        SlashCommandInfo::builtin("providers", "Manage configured providers"),
        SlashCommandInfo::builtin("clear", "Clear the output pane"),
        SlashCommandInfo::builtin("reload", "Hot-reload config and context files"),
        SlashCommandInfo::builtin("copy", "Copy the last assistant message to the clipboard"),
        SlashCommandInfo::builtin("help", "Show this list"),
        SlashCommandInfo::builtin("quit", "Quit anie"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_builtins_populates_known_commands() {
        let registry = CommandRegistry::with_builtins();
        assert!(registry.lookup("compact").is_some());
        assert!(registry.lookup("fork").is_some());
        assert!(registry.lookup("help").is_some());
    }

    #[test]
    fn all_builtins_are_tagged_builtin() {
        let registry = CommandRegistry::with_builtins();
        for command in registry.all() {
            assert!(
                matches!(command.source, SlashCommandSource::Builtin),
                "expected builtin source, got {:?} for {}",
                command.source,
                command.name
            );
        }
    }

    #[test]
    fn register_rejects_duplicates() {
        let mut registry = CommandRegistry::with_builtins();
        let err = registry
            .register(SlashCommandInfo::builtin("compact", "noop"))
            .expect_err("duplicate compact should fail");
        assert_eq!(err.name, "compact");
    }

    #[test]
    fn register_accepts_extension_command() {
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register(SlashCommandInfo {
                name: "mycmd",
                summary: "Extension command",
                source: SlashCommandSource::Extension {
                    extension_name: "my-extension".to_string(),
                },
            })
            .expect("register extension");
        let info = registry.lookup("mycmd").expect("lookup extension");
        assert!(matches!(info.source, SlashCommandSource::Extension { .. }));
    }

    #[test]
    fn source_label_formats_each_kind() {
        assert_eq!(SlashCommandSource::Builtin.label(), "builtin");
        assert_eq!(
            SlashCommandSource::Extension {
                extension_name: "foo".into()
            }
            .label(),
            "extension: foo"
        );
        assert_eq!(
            SlashCommandSource::Prompt {
                template_path: PathBuf::from("/tmp/p.md")
            }
            .label(),
            "prompt: /tmp/p.md"
        );
        assert_eq!(
            SlashCommandSource::Skill {
                skill_name: "bar".into()
            }
            .label(),
            "skill: bar"
        );
    }

    #[test]
    fn grouped_by_source_keeps_kinds_together() {
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register(SlashCommandInfo {
                name: "ext-a",
                summary: "",
                source: SlashCommandSource::Extension {
                    extension_name: "a".into(),
                },
            })
            .expect("register a");
        registry
            .register(SlashCommandInfo {
                name: "ext-b",
                summary: "",
                source: SlashCommandSource::Extension {
                    extension_name: "b".into(),
                },
            })
            .expect("register b");

        let groups = registry.grouped_by_source();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, SourceKey::Builtin);
        assert_eq!(groups[1].0, SourceKey::Extension);
        assert_eq!(groups[1].1.len(), 2);
    }
}
