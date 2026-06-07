//! Linux Landlock + seccomp backend (behind the `sandbox-linux` feature).
//!
//! PR1 stub: the real ruleset/filter installation lands in PR2 (Landlock
//! filesystem) and PR3 (seccomp network). Compiled only on Linux with the
//! feature enabled.

use crate::{SandboxError, SandboxSpec};

pub(crate) fn apply(
    cmd: &mut tokio::process::Command,
    spec: &SandboxSpec,
) -> Result<(), SandboxError> {
    let _ = (cmd, spec);
    Err(SandboxError::Unsupported(
        "linux sandbox backend not yet implemented".to_string(),
    ))
}
