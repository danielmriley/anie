//! Builtin `AutocompleteProvider` implementation that reads the
//! `SlashCommandInfo` catalog and produces command-name and
//! argument-value suggestions.
//!
//! This is the only provider anie ships today. Extension-, skill-,
//! and prompt-sourced commands feed through the same provider
//! because they all end up in the catalog that the CLI hands to
//! `App::new`. Dynamic argument sources (e.g. live model lists)
//! can be registered via `CommandCompletionProvider::new` — see
//! the `ArgumentSource` trait below.

use std::collections::HashMap;

use super::{
    AutocompleteProvider, Context, Suggestion, SuggestionKind, SuggestionSet, parse_context,
};
use crate::commands::{ArgumentSpec, SlashCommandInfo};

/// Runtime-supplied argument completions for a command whose
/// values can't be baked into the static `ArgumentSpec`.
///
/// Example future use: a `/model <id>` source that enumerates
/// the live model catalog. Plan 12 ships without any dynamic
/// sources — `/model` still routes through the full picker when
/// the user presses Enter — but the seam exists so plan 10's
/// extension system can register per-command completers without
/// touching the popup code.
pub(crate) trait ArgumentSource: Send + Sync {
    /// Completions for `prefix`. Return an empty vec to signal
    /// "no suggestions for this prefix."
    fn completions(&self, prefix: &str) -> Vec<Suggestion>;
}

/// Per-command metadata with cached lowercase forms so
/// prefix filtering in the hot keystroke path doesn't
/// lowercase the same stable strings on every comparison.
/// Plan 05 PR-C.
struct CachedCommand {
    info: SlashCommandInfo,
    name_lower: String,
    /// Pre-lowercased enumerated argument values (from
    /// `ArgumentSpec::Enumerated` or `Subcommands`). Empty
    /// for `FreeForm` and `None` specs.
    argument_values_lower: Vec<String>,
}

impl CachedCommand {
    fn from_info(info: SlashCommandInfo) -> Self {
        let name_lower = info.name.to_ascii_lowercase();
        let argument_values_lower = match &info.arguments {
            ArgumentSpec::Enumerated { values, .. } => {
                values.iter().map(|v| v.to_ascii_lowercase()).collect()
            }
            ArgumentSpec::Subcommands { known } => {
                known.iter().map(|v| v.to_ascii_lowercase()).collect()
            }
            ArgumentSpec::FreeForm { .. } | ArgumentSpec::None => Vec::new(),
        };
        Self {
            info,
            name_lower,
            argument_values_lower,
        }
    }

    /// Original-case values for building `Suggestion.value` —
    /// the popup displays the canonical string the user sees,
    /// not a lowercased one.
    fn argument_values(&self) -> &[&'static str] {
        match &self.info.arguments {
            ArgumentSpec::Enumerated { values, .. } => values,
            ArgumentSpec::Subcommands { known } => known,
            ArgumentSpec::FreeForm { .. } | ArgumentSpec::None => &[],
        }
    }
}

/// Completes command names from a `SlashCommandInfo` catalog and
/// argument values from either the static `ArgumentSpec` or a
/// registered `ArgumentSource`.
pub(crate) struct CommandCompletionProvider {
    commands: Vec<CachedCommand>,
    argument_sources: HashMap<String, Box<dyn ArgumentSource>>,
}

impl CommandCompletionProvider {
    /// Build a provider from a catalog with no dynamic argument
    /// sources. Sufficient for plan 12 phase B/C/D shipping.
    /// The provider precomputes lowercase command names and
    /// enumerated argument values once at construction time.
    #[must_use]
    pub(crate) fn new(commands: Vec<SlashCommandInfo>) -> Self {
        Self {
            commands: commands.into_iter().map(CachedCommand::from_info).collect(),
            argument_sources: HashMap::new(),
        }
    }

    /// Register a dynamic argument source keyed by command name
    /// (without the leading slash). Takes precedence over the
    /// static `ArgumentSpec` when both exist.
    #[allow(dead_code)]
    pub(crate) fn with_argument_source(
        mut self,
        name: impl Into<String>,
        source: Box<dyn ArgumentSource>,
    ) -> Self {
        self.argument_sources.insert(name.into(), source);
        self
    }

    fn command_name_suggestions(&self, prefix: &str) -> Vec<Suggestion> {
        let stripped = prefix.strip_prefix('/').unwrap_or(prefix);
        let needle = stripped.to_ascii_lowercase();
        self.commands
            .iter()
            .filter(|cmd| needle.is_empty() || cmd.name_lower.starts_with(&needle))
            .map(|cmd| Suggestion {
                value: cmd.info.name.to_string(),
                label: cmd.info.name.to_string(),
                description: description_for(&cmd.info),
            })
            .collect()
    }

    fn argument_suggestions(&self, name: &str, prefix: &str) -> Vec<Suggestion> {
        if let Some(source) = self.argument_sources.get(name) {
            return source.completions(prefix);
        }
        let Some(cmd) = self.commands.iter().find(|cmd| cmd.info.name == name) else {
            return Vec::new();
        };
        static_argument_suggestions(cmd, prefix)
    }
}

/// Prefix-match cached lowercase argument values against
/// `prefix`. Lowercases `prefix` once rather than lowercasing
/// every candidate. Emits `Suggestion`s with original-case
/// `value` fields so the popup shows canonical values.
fn static_argument_suggestions(cmd: &CachedCommand, prefix: &str) -> Vec<Suggestion> {
    let values = cmd.argument_values();
    if values.is_empty() {
        return Vec::new();
    }
    let needle = prefix.to_ascii_lowercase();
    cmd.argument_values_lower
        .iter()
        .zip(values.iter())
        .filter(|(lower, _)| needle.is_empty() || lower.starts_with(&needle))
        .map(|(_, value)| Suggestion {
            value: (*value).to_string(),
            label: (*value).to_string(),
            description: None,
        })
        .collect()
}

fn description_for(info: &SlashCommandInfo) -> Option<String> {
    let summary = (!info.summary.is_empty()).then(|| info.summary.to_string());
    match (info.argument_hint, summary) {
        (Some(hint), Some(s)) => Some(format!("{hint} — {s}")),
        (Some(hint), None) => Some(hint.to_string()),
        (None, Some(s)) => Some(s),
        (None, None) => None,
    }
}

impl AutocompleteProvider for CommandCompletionProvider {
    fn suggestions(&self, line: &str, cursor: usize) -> Option<SuggestionSet> {
        match parse_context(line, cursor, |name| {
            self.commands.iter().any(|cmd| cmd.info.name == name)
        }) {
            Context::None => None,
            Context::CommandName { prefix } => {
                let items = self.command_name_suggestions(&prefix);
                if items.is_empty() {
                    return None;
                }
                Some(SuggestionSet {
                    items,
                    prefix,
                    kind: SuggestionKind::CommandName,
                })
            }
            Context::ArgumentValue {
                name,
                argument_prefix,
            } => {
                let items = self.argument_suggestions(&name, &argument_prefix);
                if items.is_empty() {
                    return None;
                }
                Some(SuggestionSet {
                    items,
                    prefix: argument_prefix,
                    kind: SuggestionKind::ArgumentValue {
                        command_name: name,
                    },
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{ArgumentSpec, SlashCommandInfo};

    const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];

    fn catalog() -> Vec<SlashCommandInfo> {
        vec![
            SlashCommandInfo::builtin_with_args(
                "model",
                "Select model",
                ArgumentSpec::FreeForm { required: false },
                Some("[<provider:id>|<id>]"),
            ),
            SlashCommandInfo::builtin_with_args(
                "thinking",
                "Set reasoning effort",
                ArgumentSpec::Enumerated {
                    values: LEVELS,
                    required: false,
                },
                Some("[off|minimal|low|medium|high]"),
            ),
            SlashCommandInfo::builtin_with_args(
                "session",
                "Session commands",
                ArgumentSpec::Subcommands { known: &["list"] },
                Some("[list|<id>]"),
            ),
            SlashCommandInfo::builtin("compact", "Manually compact"),
            SlashCommandInfo::builtin("help", "Show help"),
        ]
    }

    #[test]
    fn empty_prefix_returns_every_command() {
        let provider = CommandCompletionProvider::new(catalog());
        let set = provider.suggestions("/", 1).expect("set");
        assert_eq!(set.kind, SuggestionKind::CommandName);
        assert_eq!(set.prefix, "/");
        let labels: Vec<_> = set.items.iter().map(|s| s.label.as_str()).collect();
        assert!(labels.contains(&"thinking"), "{labels:?}");
        assert!(labels.contains(&"model"), "{labels:?}");
        assert!(labels.contains(&"help"), "{labels:?}");
    }

    #[test]
    fn prefix_filter_is_case_insensitive() {
        let provider = CommandCompletionProvider::new(catalog());
        let set = provider.suggestions("/TH", 3).expect("set");
        let labels: Vec<_> = set.items.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["thinking"]);
    }

    #[test]
    fn thinking_argument_returns_four_values() {
        let provider = CommandCompletionProvider::new(catalog());
        let line = "/thinking ";
        let set = provider.suggestions(line, line.len()).expect("set");
        assert_eq!(
            set.kind,
            SuggestionKind::ArgumentValue {
                command_name: "thinking".into()
            }
        );
        let values: Vec<_> = set.items.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["off", "minimal", "low", "medium", "high"]);
    }

    #[test]
    fn thinking_argument_prefix_narrows_values() {
        let provider = CommandCompletionProvider::new(catalog());
        // `m` matches both `minimal` and `medium`.
        let line = "/thinking m";
        let set = provider.suggestions(line, line.len()).expect("set");
        let values: Vec<_> = set.items.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["minimal", "medium"]);

        // `me` narrows to just `medium`.
        let line = "/thinking me";
        let set = provider.suggestions(line, line.len()).expect("set");
        let values: Vec<_> = set.items.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["medium"]);
    }

    #[test]
    fn session_subcommand_returns_known_list() {
        let provider = CommandCompletionProvider::new(catalog());
        let line = "/session ";
        let set = provider.suggestions(line, line.len()).expect("set");
        let values: Vec<_> = set.items.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["list"]);
    }

    #[test]
    fn freeform_model_argument_returns_none_by_default() {
        // Plan 12 deliberately does not surface model IDs in the
        // popup — Enter on `/model` goes through the full picker.
        let provider = CommandCompletionProvider::new(catalog());
        let line = "/model gpt";
        assert!(provider.suggestions(line, line.len()).is_none());
    }

    #[test]
    fn argument_less_commands_return_no_argument_suggestions() {
        let provider = CommandCompletionProvider::new(catalog());
        let line = "/compact something";
        // `/compact` has ArgumentSpec::None — no argument source,
        // so we return None rather than an empty popup.
        assert!(provider.suggestions(line, line.len()).is_none());
    }

    #[test]
    fn description_column_includes_hint_and_summary() {
        let provider = CommandCompletionProvider::new(catalog());
        let set = provider.suggestions("/thi", 4).expect("set");
        let thinking = set
            .items
            .iter()
            .find(|s| s.label == "thinking")
            .expect("thinking suggestion");
        let desc = thinking
            .description
            .as_deref()
            .expect("description present");
        assert!(desc.contains("[off|minimal|low|medium|high]"), "{desc}");
        assert!(desc.contains("Set reasoning effort"), "{desc}");
    }

    struct StaticSource(Vec<&'static str>);

    impl ArgumentSource for StaticSource {
        fn completions(&self, prefix: &str) -> Vec<Suggestion> {
            self.0
                .iter()
                .filter(|value| value.starts_with(prefix))
                .map(|value| Suggestion {
                    value: (*value).to_string(),
                    label: (*value).to_string(),
                    description: None,
                })
                .collect()
        }
    }

    #[test]
    fn dynamic_argument_source_takes_precedence_over_static_spec() {
        let provider = CommandCompletionProvider::new(catalog()).with_argument_source(
            "thinking",
            Box::new(StaticSource(vec!["override-one", "override-two"])),
        );
        let line = "/thinking o";
        let set = provider.suggestions(line, line.len()).expect("set");
        let values: Vec<_> = set.items.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(values, vec!["override-one", "override-two"]);
    }

    #[test]
    fn unknown_command_returns_none_for_command_name_lookup() {
        let provider = CommandCompletionProvider::new(catalog());
        // `/xyz ` parses as Context::None because xyz isn't in the
        // catalog, so the whole line falls through.
        let line = "/xyz something";
        assert!(provider.suggestions(line, line.len()).is_none());
    }
}
