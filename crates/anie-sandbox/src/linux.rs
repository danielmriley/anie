//! Linux Landlock + seccomp backend (behind the `sandbox-linux` feature).
//!
//! The ruleset is compiled in the *parent* (where errors are easy to
//! surface as typed `SandboxError`), then moved into a `pre_exec` closure
//! that only calls the cheap `restrict_self()` syscall in the forked
//! child, before `exec`. A failure inside `pre_exec` aborts the spawn —
//! the child never execs — which the parent observes as a spawn error.
//!
//! Filesystem confinement (Landlock) lands here in PR2; network
//! confinement (seccomp) extends this file in PR3.

use std::io;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr,
};

use crate::{SandboxError, SandboxSpec};

const ABI_VERSION: ABI = ABI::V1;

/// Whether the running kernel supports Landlock. Detected by building a
/// `HardRequirement` ruleset (which errors when unsupported) — without
/// restricting the calling process. Test-only for now; the runtime
/// fail-closed decision is made by `apply` via the compat level.
#[cfg(test)]
pub(crate) fn landlock_available() -> bool {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI_VERSION))
        .and_then(|ruleset| ruleset.create())
        .is_ok()
}

pub(crate) fn apply(
    cmd: &mut tokio::process::Command,
    spec: &SandboxSpec,
) -> Result<(), SandboxError> {
    // Pick the compat posture. HardRequirement makes ruleset construction
    // error out on a kernel without Landlock (so we fail closed); with
    // BestEffort an unsupported kernel degrades to a no-op ruleset.
    let compat = if spec.require_kernel_support {
        CompatLevel::HardRequirement
    } else {
        CompatLevel::BestEffort
    };

    let created = match build_ruleset(spec, compat) {
        Ok(created) => created,
        // A build failure under HardRequirement is most commonly a
        // missing-kernel-support compat error → fail closed.
        Err(error) => {
            return Err(if spec.require_kernel_support {
                SandboxError::Unsupported(error)
            } else {
                SandboxError::Ruleset(error)
            });
        }
    };

    // Compile the network-deny seccomp filter in the parent (None when
    // network is allowed). Applied in the child alongside Landlock.
    let network_filter = if spec.allow_network {
        None
    } else {
        Some(build_network_deny_filter().map_err(SandboxError::Ruleset)?)
    };

    // Move the created ruleset + filter into the child's `pre_exec`.
    // `restrict_self` consumes the ruleset, so wrap in `Option`.
    let mut created = Some(created);
    // SAFETY: the closure runs post-fork, pre-exec. It performs only the
    // `restrict_self` + `seccomp(apply)` syscalls (no allocation, no
    // locks); the ruleset, path fds, and BPF program were built in the
    // parent.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(ruleset) = created.take() {
                ruleset.restrict_self().map_err(|error| {
                    io::Error::other(format!("landlock restrict_self: {error}"))
                })?;
            }
            if let Some(program) = network_filter.as_ref() {
                seccompiler::apply_filter(program)
                    .map_err(|error| io::Error::other(format!("seccomp apply: {error}")))?;
            }
            Ok(())
        });
    }
    Ok(())
}

/// Build a seccomp BPF program that denies socket creation (so the child
/// cannot open outbound network connections). Everything else is allowed;
/// a denied `socket()` returns `EACCES`.
fn build_network_deny_filter() -> Result<seccompiler::BpfProgram, String> {
    use seccompiler::{SeccompAction, SeccompFilter};

    // An empty rule vec for a syscall means "match unconditionally".
    // Denying `socket` is sufficient: no socket, no outbound connection.
    let rules = [(libc::SYS_socket, Vec::new())].into_iter().collect();
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow, // default: allow everything else
        SeccompAction::Errno(libc::EACCES as u32), // matched: socket -> EACCES
        std::env::consts::ARCH
            .try_into()
            .map_err(|error| format!("seccomp target arch: {error}"))?,
    )
    .map_err(|error| format!("seccomp filter: {error}"))?;
    filter
        .try_into()
        .map_err(|error| format!("seccomp compile: {error}"))
}

/// Build a ruleset: read everywhere, read+write under each writable root.
fn build_ruleset(spec: &SandboxSpec, compat: CompatLevel) -> Result<RulesetCreated, String> {
    let access_all = AccessFs::from_all(ABI_VERSION);
    let access_read = AccessFs::from_read(ABI_VERSION);

    let mut ruleset = Ruleset::default()
        .set_compatibility(compat)
        .handle_access(access_all)
        .map_err(|error: landlock::RulesetError| error.to_string())?
        .create()
        .map_err(|error: landlock::RulesetError| error.to_string())?;

    // Read access to the whole filesystem (workspace-write profile).
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            PathFd::new("/").map_err(|error| format!("open /: {error}"))?,
            access_read,
        ))
        .map_err(|error: landlock::RulesetError| error.to_string())?;

    // Read + write under each configured root.
    for root in &spec.writable_roots {
        let fd = PathFd::new(root).map_err(|error| format!("open {}: {error}", root.display()))?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access_all))
            .map_err(|error: landlock::RulesetError| error.to_string())?;
    }

    Ok(ruleset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    // Build a confined std Command (the integration tests don't need the
    // tokio wrapper) and return its exit success.
    fn run_confined(spec: &SandboxSpec, shell_cmd: &str) -> bool {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(shell_cmd);
        super::apply(&mut cmd, spec).expect("apply");
        // Convert to a std Command run synchronously via the same pre_exec.
        // (tokio::process::Command derefs to std for our needs.)
        let std_cmd: &mut Command = cmd.as_std_mut();
        std_cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    fn spec(writable: Vec<PathBuf>) -> SandboxSpec {
        SandboxSpec {
            writable_roots: writable,
            allow_network: true, // PR2 doesn't gate network
            require_kernel_support: true,
        }
    }

    #[test]
    fn landlock_absent_without_require_support_runs_unconfined_ok() {
        if landlock_available() {
            return; // can't simulate absence on a supporting kernel
        }
        let mut cmd = tokio::process::Command::new("true");
        let s = SandboxSpec {
            require_kernel_support: false,
            ..SandboxSpec::default()
        };
        assert!(super::apply(&mut cmd, &s).is_ok());
    }

    #[test]
    fn landlock_absent_with_require_support_returns_unsupported() {
        if landlock_available() {
            return;
        }
        let mut cmd = tokio::process::Command::new("true");
        let s = SandboxSpec::default(); // require_kernel_support = true
        assert!(matches!(
            super::apply(&mut cmd, &s),
            Err(SandboxError::Unsupported(_))
        ));
    }

    #[test]
    fn write_inside_writable_root_succeeds() {
        if !landlock_available() {
            return;
        }
        let dir = tempfile_dir();
        let target = dir.join("ok.txt");
        let ok = run_confined(&spec(vec![dir]), &format!("echo hi > {}", target.display()));
        assert!(ok, "write inside writable root should succeed");
        assert!(target.exists());
    }

    #[test]
    fn write_outside_writable_root_is_denied() {
        if !landlock_available() {
            return;
        }
        let writable = tempfile_dir();
        let outside = tempfile_dir(); // a different root, not writable
        let target = outside.join("denied.txt");
        let ok = run_confined(
            &spec(vec![writable]),
            &format!("echo hi > {}", target.display()),
        );
        assert!(!ok, "write outside writable roots must be denied");
        assert!(!target.exists());
    }

    #[test]
    fn read_outside_writable_root_still_succeeds() {
        if !landlock_available() {
            return;
        }
        let writable = tempfile_dir();
        let other = tempfile_dir();
        let readable = other.join("readme.txt");
        std::fs::write(&readable, "data").expect("seed");
        // Read to inherited stdout — no `> /dev/null` redirect, since
        // writing /dev/null is itself a write outside the writable roots.
        let ok = run_confined(
            &spec(vec![writable]),
            &format!("cat {}", readable.display()),
        );
        assert!(ok, "reads outside writable roots must still succeed");
    }

    fn net_spec(allow_network: bool, writable: Vec<PathBuf>) -> SandboxSpec {
        SandboxSpec {
            writable_roots: writable,
            allow_network,
            require_kernel_support: true,
        }
    }

    fn python3() -> Option<&'static str> {
        ["/usr/bin/python3", "/bin/python3"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
    }

    const OPEN_SOCKET: &str =
        "import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM).close()";

    #[test]
    fn network_denied_blocks_outbound_socket() {
        if !landlock_available() || python3().is_none() {
            return;
        }
        let dir = tempfile_dir();
        let ok = run_confined(
            &net_spec(false, vec![dir]),
            &format!("python3 -c '{OPEN_SOCKET}'"),
        );
        assert!(!ok, "socket creation must be denied when network is off");
    }

    #[test]
    fn network_allowed_permits_outbound_socket() {
        if !landlock_available() || python3().is_none() {
            return;
        }
        let dir = tempfile_dir();
        let ok = run_confined(
            &net_spec(true, vec![dir]),
            &format!("python3 -c '{OPEN_SOCKET}'"),
        );
        assert!(ok, "socket creation must succeed when network is allowed");
    }

    #[test]
    fn seccomp_filter_does_not_block_benign_file_io() {
        if !landlock_available() {
            return;
        }
        // Network off, but ordinary file writes inside the root still work.
        let dir = tempfile_dir();
        let target = dir.join("io.txt");
        let ok = run_confined(
            &net_spec(false, vec![dir]),
            &format!("echo hi > {} && cat {}", target.display(), target.display()),
        );
        assert!(
            ok,
            "file I/O must be unaffected by the network seccomp filter"
        );
        assert!(target.exists());
    }

    fn tempfile_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("anie-sbx-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }
}
