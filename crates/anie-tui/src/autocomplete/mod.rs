//! Inline autocomplete for the TUI input pane.
//!
//! This module owns the **data** side of autocomplete: the
//! provider trait, suggestion shape, context parser, and the
//! builtin command completer. The rendering side lives in
//! `autocomplete::popup` (plan 12 phase C); editor integration
//! lives in `input` (plan 12 phase D).
//!
//! Design mirrors pi's `packages/tui/src/autocomplete.ts`:
//! providers produce `SuggestionSet`s, the editor never builds
//! completions itself, and argument-value completion is routed
//! through the same popup as command-name completion.

pub(crate) mod command;
pub(crate) mod popup;

use crate::commands::SlashCommandInfo;

pub(crate) use command::CommandCompletionProvider;
pub(crate) use popup::AutocompletePopup;

/// A single row rendered in the autocomplete popup.
///
/// `value` is the string that replaces the current `prefix` in
/// the input buffer when the user accepts. `label` is what the
/// popup displays in the name column. `description` is an
/// optional second column (argument hint or short summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Suggestion {
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) description: Option<String>,
}

/// Whether the popup is completing a command name or an argument
/// value. The editor uses this to decide whether to append a
/// trailing space after an apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SuggestionKind {
    /// The user is typing the command name (`/` at line start, no
    /// space yet).
    CommandName,
    /// The user is typing an argument for a known command.
    ArgumentValue { command_name: String },
}

/// A batch of suggestions plus the substring they replace.
///
/// `prefix` is the text slice currently in the input that will be
/// overwritten when a suggestion is accepted. For command names
/// that's `/xyz`; for argument values that's the partial argument
/// typed so far (e.g. `me` for `/thinking me`).
#[derive(Debug, Clone)]
pub(crate) struct SuggestionSet {
    pub(crate) items: Vec<Suggestion>,
    pub(crate) prefix: String,
    pub(crate) kind: SuggestionKind,
}

/// The parsed autocomplete context for a given cursor position.
///
/// Separates context analysis from suggestion generation so we
/// can test the parser exhaustively without mocking providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Context {
    /// Cursor is inside a command name at line start. `prefix` is
    /// everything from `/` up to the cursor (including the `/`).
    CommandName { prefix: String },
    /// Cursor is in the argument region of a known command.
    /// `name` is the command name without the leading slash;
    /// `argument_prefix` is whatever has been typed after the
    /// separating whitespace, up to the cursor.
    ArgumentValue {
        name: String,
        argument_prefix: String,
    },
    /// No autocomplete applies here.
    None,
}

/// Extract the autocomplete context at `(line, cursor)`.
///
/// Rules (mirror pi's `autocomplete.ts`):
///
/// - `/` at the **start** of the line triggers command-name
///   completion. Leading whitespace before `/` disables it,
///   preventing `    /foo` (which users usually mean as literal
///   text) from popping the menu.
/// - Once the line has a space after a command name, everything
///   from that space to the cursor (or to the next whitespace,
///   whichever comes first) is the argument prefix.
/// - Commands with no known catalog entry don't trigger
///   argument-value completion.
///
/// `cursor` is a byte offset. Callers should pass the byte index
/// equal to `line.chars()` count when the cursor is at the end.
pub(crate) fn parse_context(
    line: &str,
    cursor: usize,
    known_commands: &[SlashCommandInfo],
) -> Context {
    let cursor = cursor.min(line.len());
    let before = &line[..cursor];

    // Must start with `/` at the very first byte — no leading
    // whitespace, no preceding text.
    if !before.starts_with('/') {
        return Context::None;
    }

    // Find the first whitespace after the leading `/`. Before
    // that boundary we're still in command-name mode.
    let first_space = before[1..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, _)| 1 + index);

    match first_space {
        None => Context::CommandName {
            prefix: before.to_string(),
        },
        Some(space_index) => {
            // Command name is whatever sits between the `/` and
            // the first whitespace.
            let name = &before[1..space_index];
            if name.is_empty() {
                return Context::None;
            }
            if !known_commands.iter().any(|info| info.name == name) {
                return Context::None;
            }
            // Argument prefix is the substring after the space
            // (trimming only leading whitespace preserved from the
            // command's explicit separator, not user-typed
            // whitespace).
            let argument_prefix = before[space_index..]
                .trim_start_matches(|ch: char| ch.is_whitespace())
                .to_string();
            Context::ArgumentValue {
                name: name.to_string(),
                argument_prefix,
            }
        }
    }
}

/// Producer of suggestions for an input line.
///
/// Kept synchronous for now — the builtin command provider is
/// pure CPU, and plan 12 phase D documents the escape hatch for
/// dynamic providers (spawn a request on a background task, let
/// the editor's request token drop stale results).
pub(crate) trait AutocompleteProvider: Send + Sync {
    /// Generate suggestions for `(line, cursor)`. Return `None`
    /// when no context applies, or when the resulting suggestion
    /// list would be empty.
    fn suggestions(&self, line: &str, cursor: usize) -> Option<SuggestionSet>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{ArgumentSpec, SlashCommandInfo};

    const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];

    fn catalog() -> Vec<SlashCommandInfo> {
        vec![
            SlashCommandInfo::builtin_with_args(
                "thinking",
                "Set reasoning effort",
                ArgumentSpec::Enumerated {
                    values: LEVELS,
                    required: false,
                },
                Some("[off|minimal|low|medium|high]"),
            ),
            SlashCommandInfo::builtin("compact", "Manually compact"),
        ]
    }

    fn len(s: &str) -> usize {
        s.len()
    }

    #[test]
    fn command_name_at_line_start_with_prefix() {
        let catalog = catalog();
        let ctx = parse_context("/thi", len("/thi"), &catalog);
        assert_eq!(
            ctx,
            Context::CommandName {
                prefix: "/thi".into()
            }
        );
    }

    #[test]
    fn command_name_at_line_start_with_cursor_mid_prefix() {
        let catalog = catalog();
        // Cursor between / and h: "/|thi"
        let ctx = parse_context("/thi", 1, &catalog);
        assert_eq!(
            ctx,
            Context::CommandName {
                prefix: "/".into()
            }
        );
    }

    #[test]
    fn slash_not_at_line_start_disables_context() {
        let catalog = catalog();
        let ctx = parse_context("hello /thi", len("hello /thi"), &catalog);
        assert_eq!(ctx, Context::None);

        let ctx = parse_context("   /thi", len("   /thi"), &catalog);
        assert_eq!(ctx, Context::None);
    }

    #[test]
    fn argument_context_after_known_command_with_prefix() {
        let catalog = catalog();
        let line = "/thinking me";
        let ctx = parse_context(line, len(line), &catalog);
        assert_eq!(
            ctx,
            Context::ArgumentValue {
                name: "thinking".into(),
                argument_prefix: "me".into(),
            }
        );
    }

    #[test]
    fn argument_context_empty_prefix_when_cursor_after_space() {
        let catalog = catalog();
        let line = "/thinking ";
        let ctx = parse_context(line, len(line), &catalog);
        assert_eq!(
            ctx,
            Context::ArgumentValue {
                name: "thinking".into(),
                argument_prefix: String::new(),
            }
        );
    }

    #[test]
    fn argument_context_returns_none_for_unknown_command() {
        let catalog = catalog();
        let line = "/unknown arg";
        let ctx = parse_context(line, len(line), &catalog);
        assert_eq!(ctx, Context::None);
    }

    #[test]
    fn empty_input_returns_none() {
        let catalog = catalog();
        assert_eq!(parse_context("", 0, &catalog), Context::None);
    }

    #[test]
    fn cursor_at_slash_only_matches_command_name() {
        let catalog = catalog();
        let ctx = parse_context("/", 1, &catalog);
        assert_eq!(
            ctx,
            Context::CommandName {
                prefix: "/".into()
            }
        );
    }
}
