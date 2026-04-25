//! Slash-command metadata types shared between the controller
//! (dispatch) and the TUI (display + validation + autocomplete).
//!
//! These types used to live in `anie-cli`, but the TUI needs to
//! read them in order to validate arguments before dispatch (plan
//! 11) and to render the inline autocomplete popup (plan 12).
//! Because `anie-cli` depends on `anie-tui` — not the other way
//! around — the metadata types were hoisted here.
//!
//! **What lives here:** pure data about a command (its name,
//! summary, argument shape, display hint, and origin tag).
//!
//! **What still lives in `anie-cli`:** the builtin catalog
//! (`builtin_commands()`), the central `CommandRegistry` that owns
//! ordering and `/help` rendering, and dispatch through
//! `UiAction`. Mirrors pi-mono's `slash-commands.ts` which also
//! keeps metadata and dispatch separate.

use std::path::PathBuf;

/// Where a registered slash command came from.
///
/// Non-`Builtin` variants are unused today — they'll be
/// constructed once extensions (plan 10) and prompt/skill loaders
/// land. Kept now so the API is stable when those land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommandSource {
    /// Shipped with anie; dispatched via `UiAction`.
    Builtin,
    /// Registered by an extension (plan 10).
    Extension { extension_name: String },
    /// Registered by a prompt template.
    Prompt { template_path: PathBuf },
    /// Registered by a skill.
    Skill { skill_name: String },
}

impl SlashCommandSource {
    /// Short human-readable origin label.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Builtin => "builtin".to_string(),
            Self::Extension { extension_name } => format!("extension: {extension_name}"),
            Self::Prompt { template_path } => format!("prompt: {}", template_path.display()),
            Self::Skill { skill_name } => format!("skill: {skill_name}"),
        }
    }
}

/// Declarative description of a slash command's argument shape.
///
/// Consumed by:
/// - `SlashCommandInfo::validate` (pre-dispatch validation).
/// - The `/help` printer (inline hint column).
/// - The inline autocomplete popup (plan 12) for argument-value
///   suggestions on `Enumerated` and `Subcommands` variants.
///
/// Kept `&'static` where possible so builtin entries remain
/// `const`-constructible. Extension-supplied entries that need
/// runtime-allocated values can add future variants without
/// breaking callers that match on today's set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgumentSpec {
    /// Command takes no arguments. Any trailing text is rejected.
    None,
    /// Command accepts a single free-form argument. Validation of
    /// the argument value happens at dispatch time; the spec only
    /// says whether one is required.
    FreeForm { required: bool },
    /// Command accepts one of a fixed set of values (e.g.
    /// `off|low|medium|high`). Matching is case-insensitive.
    Enumerated {
        values: &'static [&'static str],
        required: bool,
    },
    /// Command has a set of known subcommands plus a free-form
    /// fallback (e.g. `/session list` or `/session <id>`). The
    /// dispatcher performs the final interpretation; the spec
    /// exists so the autocomplete popup can surface the known
    /// subcommands.
    Subcommands { known: &'static [&'static str] },
    /// `/context-length [N|reset]`: accepts no argument for query,
    /// `reset`, or an integer accepted by Ollama's `num_ctx`.
    ContextLengthOverride,
}

/// Metadata for one registered slash command.
#[derive(Debug, Clone)]
pub struct SlashCommandInfo {
    pub name: &'static str,
    pub summary: &'static str,
    pub source: SlashCommandSource,
    /// Declarative argument shape. Drives validation and
    /// argument-value completions.
    pub arguments: ArgumentSpec,
    /// Short human-readable argument hint rendered inline by
    /// `/help` and the autocomplete popup.
    ///
    /// Convention: wrap with square brackets for optional
    /// arguments (`[off|minimal|low|medium|high]`) and angle brackets for
    /// required ones (`<provider:id>`). `None` means the command
    /// takes no arguments or the hint would be redundant with the
    /// summary.
    pub argument_hint: Option<&'static str>,
}

impl SlashCommandInfo {
    /// Convenience constructor for argument-less builtins.
    #[must_use]
    pub const fn builtin(name: &'static str, summary: &'static str) -> Self {
        Self {
            name,
            summary,
            source: SlashCommandSource::Builtin,
            arguments: ArgumentSpec::None,
            argument_hint: None,
        }
    }

    /// Builtin with an explicit argument spec and hint.
    #[must_use]
    pub const fn builtin_with_args(
        name: &'static str,
        summary: &'static str,
        arguments: ArgumentSpec,
        argument_hint: Option<&'static str>,
    ) -> Self {
        Self {
            name,
            summary,
            source: SlashCommandSource::Builtin,
            arguments,
            argument_hint,
        }
    }

    /// Validate an argument string against the declared spec.
    ///
    /// Called by the TUI before dispatching a `UiAction` so a
    /// malformed command never reaches the controller. Returns a
    /// human-readable error suitable for a system-message.
    ///
    /// `arg` is the raw substring after the command name. `None`
    /// means "no argument was provided"; `Some("")` or
    /// `Some("   ")` are treated the same way (the TUI already
    /// strips surrounding whitespace, but defensive callers may
    /// still pass whitespace-only strings).
    pub fn validate(&self, arg: Option<&str>) -> Result<(), String> {
        let normalized = arg.filter(|value| !value.trim().is_empty());
        match &self.arguments {
            ArgumentSpec::None => match normalized {
                Some(_) => Err(format!("/{} takes no arguments", self.name)),
                None => Ok(()),
            },
            ArgumentSpec::Enumerated { values, required } => match normalized {
                None if *required => Err(format!(
                    "/{} requires one of: {}",
                    self.name,
                    values.join(", ")
                )),
                None => Ok(()),
                Some(value) if values.iter().any(|v| v.eq_ignore_ascii_case(value)) => Ok(()),
                Some(value) => Err(format!(
                    "/{} does not accept '{value}' (expected: {})",
                    self.name,
                    values.join(", ")
                )),
            },
            ArgumentSpec::FreeForm { required } => match normalized {
                None if *required => Err(format!("/{} requires an argument", self.name)),
                _ => Ok(()),
            },
            // Subcommands delegate final interpretation to the
            // dispatcher — the spec exists to inform autocomplete,
            // not to gate dispatch.
            ArgumentSpec::Subcommands { .. } => Ok(()),
            ArgumentSpec::ContextLengthOverride => match normalized {
                None => Ok(()),
                Some(value) if value.eq_ignore_ascii_case("reset") => Ok(()),
                Some(value) => match value.parse::<u64>() {
                    Ok(2_048..=1_048_576) => Ok(()),
                    Ok(_) => Err(format!(
                        "/{} expects an integer from 2048 to 1048576, or reset",
                        self.name
                    )),
                    Err(_) => Err(format!(
                        "/{} does not accept '{value}' (expected: integer or reset)",
                        self.name
                    )),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVELS: &[&str] = &["off", "minimal", "low", "medium", "high"];

    fn thinking_info() -> SlashCommandInfo {
        SlashCommandInfo::builtin_with_args(
            "thinking",
            "Set reasoning effort",
            ArgumentSpec::Enumerated {
                values: LEVELS,
                required: false,
            },
            Some("[off|minimal|low|medium|high]"),
        )
    }

    #[test]
    fn validate_enumerated_rejects_unknown_value() {
        let info = thinking_info();
        let err = info.validate(Some("bogus")).expect_err("reject");
        assert!(err.contains("bogus") && err.contains("high"), "{err}");
    }

    #[test]
    fn validate_enumerated_is_case_insensitive() {
        let info = thinking_info();
        for value in ["HIGH", "High", "high"] {
            info.validate(Some(value))
                .unwrap_or_else(|e| panic!("{value}: {e}"));
        }
    }

    #[test]
    fn validate_none_rejects_trailing_argument() {
        let info = SlashCommandInfo::builtin("quit", "Quit anie");
        let err = info.validate(Some("now")).expect_err("reject");
        assert!(
            err.contains("/quit") && err.contains("no arguments"),
            "{err}"
        );
    }

    #[test]
    fn validate_none_accepts_whitespace_only_arg() {
        let info = SlashCommandInfo::builtin("quit", "Quit anie");
        info.validate(Some("   "))
            .expect("whitespace is treated as no arg");
    }

    #[test]
    fn validate_freeform_optional_allows_missing() {
        let info = SlashCommandInfo::builtin_with_args(
            "model",
            "Select model",
            ArgumentSpec::FreeForm { required: false },
            Some("<id>"),
        );
        info.validate(None).expect("freeform optional allows None");
        info.validate(Some("gpt-4o"))
            .expect("freeform accepts arbitrary value");
    }

    #[test]
    fn validate_subcommands_accepts_anything() {
        let info = SlashCommandInfo::builtin_with_args(
            "session",
            "Session commands",
            ArgumentSpec::Subcommands { known: &["list"] },
            Some("[list|<id>]"),
        );
        info.validate(None).expect("None ok");
        info.validate(Some("list")).expect("known subcommand ok");
        info.validate(Some("sess-xyz"))
            .expect("free-form id passes");
    }

    #[test]
    fn context_length_arg_spec_accepts_query_set_and_reset() {
        let info = SlashCommandInfo::builtin_with_args(
            "context-length",
            "Override Ollama context length",
            ArgumentSpec::ContextLengthOverride,
            Some("[N|reset]"),
        );

        info.validate(None).expect("query form");
        info.validate(Some("16384")).expect("set form");
        info.validate(Some("reset")).expect("reset form");
        info.validate(Some("RESET"))
            .expect("case-insensitive reset");
    }

    #[test]
    fn context_length_arg_spec_rejects_out_of_range_and_unparseable_values() {
        let info = SlashCommandInfo::builtin_with_args(
            "context-length",
            "Override Ollama context length",
            ArgumentSpec::ContextLengthOverride,
            Some("[N|reset]"),
        );

        let low = info.validate(Some("1024")).expect_err("too low");
        assert!(low.contains("2048") && low.contains("1048576"), "{low}");

        let high = info.validate(Some("1048577")).expect_err("too high");
        assert!(high.contains("2048") && high.contains("1048576"), "{high}");

        let text = info.validate(Some("wide")).expect_err("not an integer");
        assert!(text.contains("wide") && text.contains("reset"), "{text}");
    }

    #[test]
    fn source_label_covers_all_variants() {
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
}
