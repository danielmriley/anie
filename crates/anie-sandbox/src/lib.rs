//! Process sandbox for tool execution — a *separate tool-execution
//! layer* (per the arch doc's stated direction), not a tweak to anie's
//! path helpers.
//!
//! Confines only the spawned tool *child*, never the long-lived `anie`
//! host process: the platform backend installs its confinement in the
//! child via `Command::pre_exec`, before `exec`. The host stays trusted.
//!
//! Platform-agnostic [`SandboxSpec`] in, a platform-specific applier
//! ([`apply`]) out. Linux (Landlock + seccomp) is behind the
//! `sandbox-linux` feature; everything else fails closed (`Unsupported`)
//! for a configured spec and is a no-op for `None`. See
//! `docs/tool_sandbox/README.md`.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::path::PathBuf;

#[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
mod linux;

/// Platform-agnostic confinement spec, built once per tool invocation
/// from config + cwd.
///
/// Deviation from the plan's 2-field sketch: `require_kernel_support` is
/// carried here (not just in config) so [`apply`] has everything it needs
/// to decide fail-closed vs. run-unconfined without a second parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpec {
    /// Roots the child may WRITE under. Reads are allowed everywhere
    /// (a "workspace-write" profile). Empty => no writes permitted.
    pub writable_roots: Vec<PathBuf>,
    /// Allow the child to open network sockets. Default false.
    pub allow_network: bool,
    /// When the kernel lacks the required support, fail closed
    /// (`Unsupported`) rather than running unconfined. Default true —
    /// the safety-correct posture.
    pub require_kernel_support: bool,
}

impl Default for SandboxSpec {
    fn default() -> Self {
        Self {
            writable_roots: Vec::new(),
            allow_network: false,
            require_kernel_support: true,
        }
    }
}

/// Typed sandbox errors — surfaced into `anie_agent::ToolError` by the
/// caller, never string-matched.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    /// The platform/kernel/build cannot provide the requested
    /// confinement, and `require_kernel_support` is set.
    #[error("sandbox unsupported on this platform/kernel: {0}")]
    Unsupported(String),
    /// Building the confinement ruleset/filter failed.
    #[error("failed to build sandbox ruleset: {0}")]
    Ruleset(String),
}

/// Install confinement into `cmd` so it applies to the spawned child
/// only. A no-op (`Ok`) when `spec` is `None`. On a platform/build
/// without backend support, the documented degrade-vs-fail knob applies:
/// `require_kernel_support == true` (the default) returns `Unsupported`
/// (fail closed), while `false` returns `Ok` and runs the child
/// unconfined.
///
/// # Errors
/// Returns [`SandboxError`] when the spec cannot be applied.
pub fn apply(
    cmd: &mut tokio::process::Command,
    spec: Option<&SandboxSpec>,
) -> Result<(), SandboxError> {
    let Some(spec) = spec else {
        return Ok(());
    };

    #[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
    {
        linux::apply(cmd, spec)
    }

    #[cfg(not(all(target_os = "linux", feature = "sandbox-linux")))]
    {
        let _ = cmd;
        // Honor the documented degrade-vs-fail knob off the Linux backend
        // too: when the caller opted into running unconfined on missing
        // support, do so rather than hard-failing every command.
        if spec.require_kernel_support {
            Err(SandboxError::Unsupported(
                "built without `sandbox-linux` support (Linux only)".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_disabled_apply_leaves_command_unmodified() {
        // `None` spec is a no-op — the command spawns exactly as today.
        let mut cmd = tokio::process::Command::new("true");
        assert!(apply(&mut cmd, None).is_ok());
    }

    #[test]
    fn spec_some_without_backend_feature_returns_unsupported() {
        // The default build has no `sandbox-linux` feature, so a
        // configured spec fails closed rather than running unconfined.
        let spec = SandboxSpec {
            writable_roots: vec![PathBuf::from("/tmp")],
            ..SandboxSpec::default()
        };
        let mut cmd = tokio::process::Command::new("true");
        let result = apply(&mut cmd, Some(&spec));
        #[cfg(not(all(target_os = "linux", feature = "sandbox-linux")))]
        assert!(
            matches!(result, Err(SandboxError::Unsupported(_))),
            "{result:?}"
        );
        // (With the backend feature on a Landlock kernel this would be Ok;
        // that path is covered by the linux backend tests.)
        #[cfg(all(target_os = "linux", feature = "sandbox-linux"))]
        let _ = result;
    }

    #[cfg(not(all(target_os = "linux", feature = "sandbox-linux")))]
    #[test]
    fn require_kernel_support_false_degrades_to_unconfined_off_backend() {
        // The documented degrade-vs-fail knob must work off the Linux
        // backend too: opt-out of strictness => run unconfined, not fail.
        let spec = SandboxSpec {
            writable_roots: vec![PathBuf::from("/tmp")],
            require_kernel_support: false,
            ..SandboxSpec::default()
        };
        let mut cmd = tokio::process::Command::new("true");
        assert!(apply(&mut cmd, Some(&spec)).is_ok());
    }

    #[test]
    fn writable_roots_default_spec_is_empty_not_root() {
        let spec = SandboxSpec::default();
        assert!(spec.writable_roots.is_empty());
        assert!(!spec.allow_network);
        assert!(spec.require_kernel_support);
    }
}
