//! Harness-mode profile: which capabilities the agent harness
//! exposes to the model on a given run.
//!
//! Three profiles, picked via `--harness-mode <mode>`:
//!
//! - `baseline` — the model with no anie features. No tools,
//!   no compaction gate, no policy hooks. Mirrors what the
//!   raw provider would do given the same prompt. Used as the
//!   floor for measurement: anything our harness adds should
//!   register as a delta against this.
//! - `current` (default) — anie's existing behavior: full
//!   tool set, controller-side compaction gate, no RLM
//!   features. This is what users run today.
//! - `rlm` — context virtualization (Plan 06 of
//!   `docs/rlm_2026-04-29/`). Currently identical to
//!   `current` on this branch; later commits add the
//!   `recurse` tool, the active-context policy, and the
//!   indexed external store as the RLM phases land.
//!
//! The mode is set at run start and is immutable for the
//! life of the run. The CLI flag (`--harness-mode`) is the
//! sole entry point; defaults to `current` for backward
//! compatibility.
//!
//! Plan: `docs/rlm_2026-04-29/07_evaluation_harness.md`.

use clap::ValueEnum;

/// The three harness-mode profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum HarnessMode {
    /// No tools, no compaction gate, no policy hooks. Floor
    /// for measurement.
    Baseline,
    /// Default — anie's existing behavior (tools + compaction
    /// gate). Backward-compatible with all prior versions.
    #[default]
    Current,
    /// Context virtualization (Plan 06). Identical to
    /// `Current` until the recurse tool + active-context
    /// policy land in subsequent commits on this branch.
    Rlm,
}

impl HarnessMode {
    /// True if the harness should register the standard tool
    /// set (read/write/edit/bash/grep/find/ls + web). False
    /// for `Baseline` mode, which exposes no tools so the
    /// model is restricted to its raw response.
    #[must_use]
    pub fn registers_tools(self) -> bool {
        !matches!(self, Self::Baseline)
    }

    /// True if the harness should install the controller-side
    /// compaction gate. False for `Baseline` mode, which
    /// makes the model fully responsible for not exceeding
    /// its context window.
    #[must_use]
    pub fn installs_compaction_gate(self) -> bool {
        !matches!(self, Self::Baseline)
    }

    /// True if the harness should install the RLM-specific
    /// `recurse` tool and the context-virtualization
    /// policy. Only true for `Rlm`. The tool and policy
    /// don't yet exist on this branch; this method exists
    /// now so later commits can flip behavior on a single
    /// boolean rather than touching every branch.
    #[must_use]
    pub fn installs_rlm_features(self) -> bool {
        matches!(self, Self::Rlm)
    }

    /// Stable string label for tracing fields and log lines.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Current => "current",
            Self::Rlm => "rlm",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default mode is `Current`, preserving backward
    /// compatibility for users who don't pass the flag.
    #[test]
    fn default_is_current() {
        assert_eq!(HarnessMode::default(), HarnessMode::Current);
    }

    /// `Baseline` opts out of all anie features. The other
    /// modes register tools.
    #[test]
    fn baseline_does_not_register_tools_or_gate() {
        assert!(!HarnessMode::Baseline.registers_tools());
        assert!(!HarnessMode::Baseline.installs_compaction_gate());
        assert!(!HarnessMode::Baseline.installs_rlm_features());

        assert!(HarnessMode::Current.registers_tools());
        assert!(HarnessMode::Current.installs_compaction_gate());
        assert!(!HarnessMode::Current.installs_rlm_features());

        assert!(HarnessMode::Rlm.registers_tools());
        assert!(HarnessMode::Rlm.installs_compaction_gate());
        assert!(HarnessMode::Rlm.installs_rlm_features());
    }

    /// Labels are stable strings used in tracing fields and
    /// log lines.
    #[test]
    fn labels_are_stable() {
        assert_eq!(HarnessMode::Baseline.label(), "baseline");
        assert_eq!(HarnessMode::Current.label(), "current");
        assert_eq!(HarnessMode::Rlm.label(), "rlm");
    }
}
