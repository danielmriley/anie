//! Slash-command registry — the CLI-side container that owns
//! ordering, dispatch coupling, and `/help` rendering.
//!
//! The metadata types (`SlashCommandInfo`, `SlashCommandSource`,
//! `ArgumentSpec`) live in `anie-tui::commands` so the TUI can
//! read them for validation (plan 11) and inline autocomplete
//! (plan 12). This module owns the builtin catalog and the
//! registry that dispatches through `UiAction`.
//!
//! **Dispatch is still NOT owned by this module.** `UiAction`
//! handling happens in the controller. This module is pure
//! catalog + convenience helpers for presentation. Mirrors
//! pi-mono's split between `slash-commands.ts` (catalog) and
//! `interactive-mode.ts` (dispatch).
//!
//! When extensions (plan 10) arrive, they register their commands
//! into this registry with `SlashCommandSource::Extension` and
//! dispatch through the extension host — not through `UiAction`.

pub(crate) use anie_tui::{ArgumentSpec, SlashCommandInfo, SlashCommandSource};

/// Accepted values for `/thinking`.
///
/// Shared by the builtin catalog and the autocomplete argument
/// source (plan 12).
pub(crate) const THINKING_LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];

/// Known subcommands for `/session`.
pub(crate) const SESSION_SUBCOMMANDS: &[&str] = &["list"];

/// Accepted values for `/markdown`.
pub(crate) const MARKDOWN_SWITCHES: &[&str] = &["on", "off"];

/// Accepted values for `/tool-output`. Plan 09 PR-C.
pub(crate) const TOOL_OUTPUT_MODES: &[&str] = &["verbose", "compact"];

/// Providers that expose OAuth login (and are valid arguments
/// to `/login` / `/logout`). Mirrors the registry in
/// `anie-cli::login_command::build_oauth_provider`.
pub(crate) const OAUTH_PROVIDERS: &[&str] = &[
    "anthropic",
    "openai-codex",
    "github-copilot",
    "google-antigravity",
    "google-gemini-cli",
];

/// All slash commands known to this anie process.
///
/// Populated at startup with `builtin_commands()`; future
/// extension systems will add their own entries.
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

    /// Look up a command by exact name.
    ///
    /// Used by tests today and by plan 12's autocomplete provider
    /// when it needs to resolve a command name pulled from the
    /// input buffer.
    #[allow(dead_code)]
    pub(crate) fn lookup(&self, name: &str) -> Option<&SlashCommandInfo> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// All registered commands, in registration order.
    pub(crate) fn all(&self) -> &[SlashCommandInfo] {
        &self.commands
    }

    /// Group registered commands by source. Order within each group
    /// is the original registration order.
    pub(crate) fn grouped_by_source(&self) -> Vec<(SourceKey, Vec<&SlashCommandInfo>)> {
        use std::path::PathBuf;
        let source_order = [
            SlashCommandSource::Builtin,
            SlashCommandSource::Extension {
                extension_name: String::new(),
            },
            SlashCommandSource::Prompt {
                template_path: PathBuf::new(),
            },
            SlashCommandSource::Skill {
                skill_name: String::new(),
            },
        ];

        source_order
            .iter()
            .filter_map(|source| {
                let key = SourceKey::from(source);
                let entries = self
                    .commands
                    .iter()
                    .filter(|command| SourceKey::from(&command.source) == key)
                    .collect::<Vec<_>>();
                if entries.is_empty() {
                    None
                } else {
                    Some((key, entries))
                }
            })
            .collect()
    }

    /// Render the `/help` output grouped by command source.
    ///
    /// Argument-hint column width is computed dynamically from the
    /// longest hint so short hints don't push the summary column
    /// out of alignment when rare long hints exist.
    pub(crate) fn format_help(&self) -> String {
        let mut out = String::from("Commands:\n");
        if self.all().is_empty() {
            return out;
        }
        let hint_width = self
            .all()
            .iter()
            .filter_map(|info| info.argument_hint)
            .map(|hint| hint.chars().count())
            .max()
            .unwrap_or(0);
        for (key, entries) in self.grouped_by_source() {
            out.push_str("  ");
            out.push_str(group_heading(key));
            out.push_str(":\n");
            for info in entries {
                let hint = info.argument_hint.unwrap_or("");
                if hint_width > 0 {
                    out.push_str(&format!(
                        "    /{:<12} {:<hint_width$}  {}\n",
                        info.name,
                        hint,
                        info.summary,
                        hint_width = hint_width,
                    ));
                } else {
                    out.push_str(&format!("    /{:<12} {}\n", info.name, info.summary));
                }
            }
        }
        out
    }

    /// Register a command. Duplicates (by name) are rejected — the
    /// first registration wins, matching pi's behavior.
    #[cfg(test)]
    pub(crate) fn register(&mut self, command: SlashCommandInfo) -> Result<(), DuplicateCommand> {
        if self.commands.iter().any(|c| c.name == command.name) {
            return Err(DuplicateCommand {
                name: command.name.to_string(),
            });
        }
        self.commands.push(command);
        Ok(())
    }
}

fn group_heading(key: SourceKey) -> &'static str {
    match key {
        SourceKey::Builtin => "Builtin",
        SourceKey::Extension => "Extensions",
        SourceKey::Prompt => "Prompts",
        SourceKey::Skill => "Skills",
    }
}

/// Coarse grouping key for `grouped_by_source` — preserves source
/// kind but drops per-origin fields so all builtins group together,
/// all extensions group together, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct DuplicateCommand {
    pub name: String,
}

/// The builtin anie slash-command catalog.
///
/// Keep this in sync with the slash-command handling split across:
/// - `anie-tui::app::handle_slash_command`
/// - `anie-tui::app::UiAction`
/// - `controller::InteractiveController::handle_action`
///
/// The test `registry_covers_every_dispatched_slash_command`
/// enforces the coupling.
///
/// Adding a builtin:
///   1. Add or reuse the `UiAction` variant in `anie-tui::app`.
///   2. Add a `SlashCommandInfo::builtin(...)` or
///      `builtin_with_args(...)` entry here.
///   3. Handle it in `InteractiveController::handle_action` if it is
///      controller-owned.
///   4. Add the name to the `dispatched` list in the coverage test.
///
/// The TUI reads this catalog (via `App::new`) to validate argument
/// shapes before dispatch and, later, to render autocomplete.
fn builtin_commands() -> Vec<SlashCommandInfo> {
    vec![
        SlashCommandInfo::builtin_with_args(
            "model",
            "Select model (opens picker on no args)",
            ArgumentSpec::FreeForm { required: false },
            Some("[<provider:id>|<id>]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "thinking",
            "Set reasoning effort",
            ArgumentSpec::Enumerated {
                values: THINKING_LEVELS,
                required: false,
            },
            Some("[off|minimal|low|medium|high]"),
        ),
        SlashCommandInfo::builtin("compact", "Manually compact the session context"),
        SlashCommandInfo::builtin("fork", "Create a child session branched from now"),
        SlashCommandInfo::builtin("diff", "Show file changes made in this session"),
        SlashCommandInfo::builtin("new", "Start a fresh session"),
        SlashCommandInfo::builtin_with_args(
            "session",
            "Show session info, list sessions, or switch",
            ArgumentSpec::Subcommands {
                known: SESSION_SUBCOMMANDS,
            },
            Some("[list|<id>]"),
        ),
        SlashCommandInfo::builtin("tools", "List active tools"),
        SlashCommandInfo::builtin("onboard", "Reopen the onboarding flow"),
        SlashCommandInfo::builtin("providers", "Manage configured providers"),
        SlashCommandInfo::builtin("clear", "Clear the output pane"),
        SlashCommandInfo::builtin("reload", "Hot-reload config and context files"),
        SlashCommandInfo::builtin("copy", "Copy the last assistant message to the clipboard"),
        SlashCommandInfo::builtin_with_args(
            "markdown",
            "Toggle markdown rendering for finalized messages",
            ArgumentSpec::Enumerated {
                values: MARKDOWN_SWITCHES,
                required: false,
            },
            Some("[on|off]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "tool-output",
            "Set tool-output display mode (verbose or compact)",
            ArgumentSpec::Enumerated {
                values: TOOL_OUTPUT_MODES,
                required: false,
            },
            Some("[verbose|compact]"),
        ),
        SlashCommandInfo::builtin_with_args(
            "login",
            "Show instructions for OAuth login against a provider",
            ArgumentSpec::Enumerated {
                values: OAUTH_PROVIDERS,
                required: true,
            },
            Some("<provider>"),
        ),
        SlashCommandInfo::builtin_with_args(
            "logout",
            "Remove a stored OAuth or API-key credential",
            ArgumentSpec::FreeForm { required: true },
            Some("<provider>"),
        ),
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
                arguments: ArgumentSpec::None,
                argument_hint: None,
            })
            .expect("register extension");
        let info = registry.lookup("mycmd").expect("lookup extension");
        assert!(matches!(info.source, SlashCommandSource::Extension { .. }));
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
                arguments: ArgumentSpec::None,
                argument_hint: None,
            })
            .expect("register a");
        registry
            .register(SlashCommandInfo {
                name: "ext-b",
                summary: "",
                source: SlashCommandSource::Extension {
                    extension_name: "b".into(),
                },
                arguments: ArgumentSpec::None,
                argument_hint: None,
            })
            .expect("register b");

        let groups = registry.grouped_by_source();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, SourceKey::Builtin);
        assert_eq!(groups[1].0, SourceKey::Extension);
        assert_eq!(groups[1].1.len(), 2);
    }

    #[test]
    fn format_help_starts_with_commands_heading() {
        let registry = CommandRegistry::with_builtins();
        assert!(registry.format_help().starts_with("Commands:\n"));
    }

    #[test]
    fn format_help_includes_every_builtin_name() {
        let registry = CommandRegistry::with_builtins();
        let help = registry.format_help();
        for command in registry.all() {
            assert!(help.contains(&format!("/{}", command.name)));
        }
    }

    #[test]
    fn format_help_renders_extensions_section_when_registered() {
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register(SlashCommandInfo {
                name: "ext-help",
                summary: "Extension help",
                source: SlashCommandSource::Extension {
                    extension_name: "demo".into(),
                },
                arguments: ArgumentSpec::None,
                argument_hint: None,
            })
            .expect("register extension command");

        let help = registry.format_help();
        assert!(help.contains("  Extensions:\n"));
        assert!(help.contains("/ext-help"));
    }

    #[test]
    fn format_help_omits_empty_sections() {
        let registry = CommandRegistry::with_builtins();
        let help = registry.format_help();
        assert!(!help.contains("Extensions:"));
        assert!(!help.contains("Prompts:"));
        assert!(!help.contains("Skills:"));
    }

    #[test]
    fn format_help_renders_argument_hint_column() {
        let registry = CommandRegistry::with_builtins();
        let help = registry.format_help();
        assert!(
            help.contains("/thinking") && help.contains("[off|minimal|low|medium|high]"),
            "expected thinking row with hint column, got:\n{help}"
        );
        assert!(
            help.contains("/model") && help.contains("[<provider:id>|<id>]"),
            "expected model row with hint column, got:\n{help}"
        );
        assert!(
            help.contains("/session") && help.contains("[list|<id>]"),
            "expected session row with hint column, got:\n{help}"
        );
    }

    #[test]
    fn registry_covers_every_dispatched_slash_command() {
        let dispatched = [
            "model",
            "thinking",
            "compact",
            "fork",
            "diff",
            "new",
            "session",
            "tools",
            "onboard",
            "providers",
            "clear",
            "reload",
            "copy",
            "markdown",
            "tool-output",
            "login",
            "logout",
            "help",
            "quit",
        ];
        let registry = CommandRegistry::with_builtins();
        for name in dispatched {
            assert!(
                registry.lookup(name).is_some(),
                "registry missing builtin '{name}' — update builtin_commands() when adding a slash command"
            );
        }
    }

    #[test]
    fn builtin_catalog_includes_argument_spec_for_every_command() {
        type SpecCheck = fn(&ArgumentSpec) -> bool;
        let registry = CommandRegistry::with_builtins();
        let expected: &[(&str, SpecCheck)] = &[
            ("thinking", |spec| {
                matches!(spec, ArgumentSpec::Enumerated { values, required: false } if *values == THINKING_LEVELS)
            }),
            ("model", |spec| {
                matches!(spec, ArgumentSpec::FreeForm { required: false })
            }),
            ("session", |spec| {
                matches!(spec, ArgumentSpec::Subcommands { known } if *known == SESSION_SUBCOMMANDS)
            }),
            ("markdown", |spec| {
                matches!(spec, ArgumentSpec::Enumerated { values, required: false } if *values == MARKDOWN_SWITCHES)
            }),
            ("tool-output", |spec| {
                matches!(spec, ArgumentSpec::Enumerated { values, required: false } if *values == TOOL_OUTPUT_MODES)
            }),
            ("login", |spec| {
                matches!(spec, ArgumentSpec::Enumerated { values, required: true } if *values == OAUTH_PROVIDERS)
            }),
            ("logout", |spec| matches!(spec, ArgumentSpec::FreeForm { required: true })),
            ("compact", |spec| matches!(spec, ArgumentSpec::None)),
            ("help", |spec| matches!(spec, ArgumentSpec::None)),
            ("quit", |spec| matches!(spec, ArgumentSpec::None)),
        ];
        for (name, check) in expected {
            let info = registry.lookup(name).expect("lookup builtin");
            assert!(
                check(&info.arguments),
                "unexpected ArgumentSpec for /{name}: {:?}",
                info.arguments
            );
        }
    }
}
