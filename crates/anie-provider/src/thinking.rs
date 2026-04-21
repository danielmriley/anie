use serde::{Deserialize, Serialize};

/// Requested reasoning / thinking level for a provider call.
///
/// Variant ordering (`Off < Minimal < Low < Medium < High`) is
/// observable via `PartialOrd` / `Ord` and matches the natural
/// reasoning-budget progression. If you insert a new level,
/// preserve this ordering.
#[derive(
    Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord,
)]
pub enum ThinkingLevel {
    /// Disable reasoning-specific features.
    #[default]
    Off,
    /// Minimal reasoning — requested via `reasoning_effort:
    /// "minimal"`. GPT-5 family accepts this; providers without
    /// `supportsReasoningEffort` fall through to the
    /// non-reasoning path unchanged.
    Minimal,
    /// Low reasoning budget.
    Low,
    /// Medium reasoning budget.
    Medium,
    /// High reasoning budget.
    High,
}
