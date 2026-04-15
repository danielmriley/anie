use serde::{Deserialize, Serialize};

/// Requested reasoning / thinking level for a provider call.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ThinkingLevel {
    /// Disable reasoning-specific features.
    #[default]
    Off,
    /// Low reasoning budget.
    Low,
    /// Medium reasoning budget.
    Medium,
    /// High reasoning budget.
    High,
}
