# Process sandbox for tool execution

Status: **planned** (not started). Initiative #7 from
`docs/rival_analysis_2026-06-06/README.md`. This folder is the plan;
no source, ROADMAP, or arch-doc edits have been made yet — those are
described here as PR steps.

Topic dir: `docs/tool_sandbox/`. Single-file plan (index + full plan),
mirroring `docs/max_tokens_handling/README.md`.

---

## 1. Rationale

### 1a. The coupling you should read first: approval layer ≠ sandbox, but it gates it

This plan covers the **process sandbox** (initiative #7). It deliberately
does **not** cover the **tool approval / permission layer** (initiative #1).
That separation is a scoping decision, not an endorsement of doing the
sandbox first. The evidence says the opposite:

- The approval layer is ranked **#1 (impact 5 ÷ effort 3)**; the sandbox is
  ranked **#7 (impact 4 ÷ effort 5)** —
  `docs/rival_analysis_2026-06-06/README.md` shortlist table.
- The plumbing for approvals already exists and is only test-wired:
  `BeforeToolCallHook` trait (`crates/anie-agent/src/hooks.rs:38-46`),
  `BeforeToolCallResult::{Allow, Block}` (`hooks.rs:17-24`), the
  `before_tool_call_hook: Option<Arc<dyn BeforeToolCallHook>>` field on
  `AgentLoopConfig`, the invocation/branch in the tool-execution path
  (`crates/anie-agent/src/agent_loop.rs:1466-1486`), and the test-only
  `#[cfg(test)] with_hooks()` setter. No production code installs a hook;
  `AgentLoopConfig::new()` sets it to `None`. (Finding `ARCH-1` / `POLICY-1`,
  `docs/rival_analysis_2026-06-06/findings_by_lens.json`,
  lens `sandbox-approvals`.)
- The approval layer delivers **most user-perceived safety** for a fraction
  of the cost, because it is mostly integration of an existing seam (policy
  impl + a TUI modal + config modes), whereas the sandbox is genuinely
  expensive and platform-specific.

**Recommendation (carried into Exit criteria as a flagged dependency):** the
approval layer should land **as a companion or prerequisite** to this work.
Concretely, the sandbox needs an *escalation* decision point — "this command
needs network / needs to write outside the workspace; run it unsandboxed /
with a relaxed profile **once**?" — and the natural place to ask that
question is exactly the `BeforeToolCallHook` seam. Building sandbox
escalation with no approval UI means the sandbox can only ever *silently*
fail-closed (return a typed error to the model) or be globally toggled in
config. That is a usable v1 (and is what this plan ships), but it leaves the
high-leverage half of tool safety on the table. **This plan ships the
fail-closed / config-toggle sandbox; it explicitly defers interactive
escalation to the approval-layer initiative and is designed to plug into it
(§2e, Deferred §8).**

### 1b. The gap this plan closes

`BashTool` spawns a shell with anie's full user security context and **no
isolation**:

- The struct doc says it outright: *"The command is not sandboxed."*
  (`crates/anie-tools/src/bash.rs:21`).
- The spawn path is a bare `tokio::process::Command`:
  `Command::new(shell).args(...).current_dir(self.cwd)...
  .kill_on_drop(true)` with `process_group(0)` on Unix
  (`crates/anie-tools/src/bash.rs:104-111`). The only "isolation" is
  `kill_on_drop` + SIGKILL-on-cancel (`kill_process_tree`, `bash.rs:349-365`).
- `BashPolicy` (`bash.rs:28-52`) is a **pre-spawn textual deny guardrail**,
  explicitly *"an accidental-risk guardrail, not a sandbox"* and *bypassable
  via shell indirection* (`bash.rs:28-31`; arch doc
  `docs/arch/anie-rs_architecture.md:459-463`). It even skips wrapper
  commands (`sudo`, `env`, `nohup`, `time`, `command` — `bash.rs:288`).
  Findings `SANDBOX-1` and `BYPASS-1` confirm this is intentional and a
  genuine gap, not an oversight.

The architecture doc already states the **direction** this plan must follow:

> "Future isolation work should be designed as a separate tool-execution
> layer. The preferred direction is WASM/containerized tool execution
> rather than quietly changing today's path resolver into a partial
> sandbox." — `docs/arch/anie-rs_architecture.md:473-475`

So: a **separate layer**, not a tweak to the path helpers. This plan honors
that by introducing a dedicated `anie-sandbox` crate rather than threading
OS-isolation logic into `anie-tools` (whose workspace charter explicitly
excludes *"sandbox policy beyond its path behavior"* —
`docs/arch/anie-rs_architecture.md:42`).

### 1c. What rivals do (and what we are matching, conservatively)

Per the analysis (treat the Codex specifics as **reported / speculative**,
not verified here — the pi tree and the codex source are not on this
machine; only `docs/anie_vs_pi_comparison.md` and the analysis summaries
are available):

- **Codex** *(reported, `SANDBOX-1` rival_baseline)*: shell commands go
  through `ExecPolicy → SandboxManager`; Linux uses **Landlock + bubblewrap**,
  Windows uses **restricted tokens**, macOS uses **seatbelt**; network is
  proxied.
- **Claude Code** *(reported)*: permission modes in settings; sandbox
  mechanism not visible from the cited material.

We **match the confirmed-finding scope, not the full Codex matrix**: a
Linux-first, opt-in filesystem + network confinement for the `bash` tool.
macOS (seatbelt) and Windows (restricted tokens) are documented as future
work (§8). Network proxying is **out of scope**; we offer a coarse
network on/off, which is the conservative, small-shape choice.

---

## 2. Design

### 2a. New crate: `anie-sandbox`

A separate crate is the "separate tool-execution layer" the arch doc asks
for, and keeps OS-isolation out of `anie-tools`. It depends only on
`anie-protocol`-free primitives (it has no protocol/session types); it
exposes a platform-agnostic **spec** and a platform-specific **applier**.

```text
anie-tools
  -> anie-sandbox     (new edge)
anie-cli
  -> anie-sandbox     (new edge: builds the spec from config + cwd)
```

`anie-sandbox` has **no** dependency on `anie-tools`, `anie-agent`, or
`anie-session` — it is a leaf utility crate.

### 2b. The spec (platform-agnostic, deliberately small)

```rust
/// Platform-agnostic description of the confinement to apply to a
/// child process. Built once per tool invocation from config + cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxSpec {
    /// Roots the child may WRITE under. Reads are allowed everywhere
    /// (matching a "workspace-write" profile). Empty => no writes.
    pub writable_roots: Vec<PathBuf>,
    /// Allow the child to open network sockets. Default false.
    pub allow_network: bool,
}
```

One coherent profile — **read-anywhere, write-under-roots, network-off** —
chosen over a `mode` enum to keep the shape minimal (project principle:
"Small shapes are how the project stays extensible"). A read-only profile
and a danger/full-access profile are **deferred** (§8); they are additive
later. `writable_roots` defaults to `[cwd]` (plus the OS temp dir) when the
operator does not override it — derived in `anie-cli`, not hardcoded in the
crate.

`SandboxSpec::disabled()` (or `Option<SandboxSpec>` = `None`) means "do
nothing," preserving today's behavior byte-for-byte.

### 2c. The applier (child-only, via `pre_exec`)

The clean integration is to confine **only the child**, never the long-lived
`anie` process. On Linux that means installing the Landlock ruleset and the
seccomp filter **in the forked child, before `exec`**, via the
`pre_exec` closure that `tokio::process::Command` re-exposes (unsafe, unix).

```rust
/// Install confinement into `cmd` so it applies to the spawned child only.
/// No-op (Ok) when the spec is None or the target is unsupported.
pub fn apply(cmd: &mut tokio::process::Command, spec: Option<&SandboxSpec>)
    -> Result<(), SandboxError>;
```

- The Landlock *ruleset* and the seccomp *BPF program* are compiled in the
  **parent** (where errors are easy to surface as typed `SandboxError`), then
  moved into the `pre_exec` closure which only calls the cheap, must-not-fail
  `restrict_self()` / `seccomp(apply)` syscalls in the child. A failure
  inside `pre_exec` aborts the spawn (the child never execs), which the
  parent observes as a spawn `io::Error` → typed error (§2d).
- Kernel-capability detection (Landlock ABI present?) happens in the parent
  at `apply()` time. The behavior when the kernel lacks Landlock is a
  **deliberate, configurable choice** (`require_kernel_support`, default
  **true** = fail-closed): refuse to spawn with a typed
  `SandboxError::Unsupported` rather than silently running unconfined. This
  is the safety-correct default and is called out as an anie deviation from
  a "best-effort" posture.

`apply` is feature-gated: a `sandbox-linux` cargo feature pulls in the
Linux backend; without it (or on non-Linux), `apply` with a `Some(spec)` and
`enabled` returns `SandboxError::Unsupported` (fail-closed) and with `None`
is a no-op.

### 2d. Typed errors (no string-matching)

New `SandboxError` in `anie-sandbox`, surfaced into the existing
`ToolError` taxonomy (`crates/anie-agent/src/tool.rs:124-136`). Rather than
inventing a stringly path, `bash.rs` maps `SandboxError` →
`ToolError::ExecutionFailed(...)` **with a stable, machine-checkable
prefix** for now (the taxonomy has only `ExecutionFailed/Aborted/Timeout`
today). A dedicated `ToolError::SandboxSetup(...)` variant is the cleaner
landing and is included as the **first commit of PR 5** (a separable
1-variant refactor; project principle: "Look for separable refactors").

```rust
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    #[error("sandbox unsupported on this platform/kernel: {0}")]
    Unsupported(String),
    #[error("failed to build sandbox ruleset: {0}")]
    Ruleset(String),
}
```

### 2e. Config surface: `[tools.sandbox]`

Extends the existing `ToolsConfig` (`crates/anie-config/src/lib.rs:98-105`)
beside `bash` and `web`, mirroring the established `BashToolConfig` /
`WebToolConfig` pattern (default impls, `#[serde(default)]`, partial-config
merge). **Disabled by default** — opt-in, exactly like
`web.allow_private_ips`.

```toml
# [tools.sandbox]
# enabled = false                 # opt-in; default false
# writable_roots = []             # empty => [cwd, $TMPDIR] derived at runtime
# allow_network = false
# require_kernel_support = true    # fail-closed if Landlock absent
```

```rust
pub struct SandboxToolConfig {
    pub enabled: bool,              // default false
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    pub allow_network: bool,        // default false
    pub require_kernel_support: bool, // default true
}
```

This is **config**, not session state. It is **not** persisted to the
session JSONL and is **not** part of `CURRENT_SESSION_SCHEMA_VERSION`
(currently `4`, `crates/anie-session/src/lib.rs:90`). **No schema bump.**
Tool-result *provenance* (whether a given bash result ran sandboxed) is
recorded only in the freeform `details` JSON of the existing `ToolResult`
(see `bash.rs` `text_result(... serde_json::json!({...}))`,
`bash.rs:226-233`) — freeform JSON needs no schema version change. (A typed
persisted provenance field is **deferred**, §8.)

### 2f. Escalation hook (design seam only, not built here)

`apply()` and `SandboxSpec` are shaped so the future approval layer can
request a relaxed spec for a single call: the `BeforeToolCallHook` would
return an approved `SandboxSpec` override (e.g. `allow_network = true` once)
that `bash.rs` passes to `apply`. This plan does **not** wire that path; it
only avoids foreclosing it (the spec is per-invocation, not baked into the
`BashTool` at construction).

---

## 3. Files to touch

New:

- `crates/anie-sandbox/Cargo.toml` — new crate, `sandbox-linux` feature.
- `crates/anie-sandbox/src/lib.rs` — `SandboxSpec`, `SandboxError`,
  `apply()`, capability detection, no-op path.
- `crates/anie-sandbox/src/linux.rs` — Landlock + seccomp backend
  (`#[cfg(target_os = "linux")]`, behind `sandbox-linux`).

Modified:

- `Cargo.toml` (workspace) — register `anie-sandbox` member + new
  `[workspace.dependencies]` lines (§ deps).
- `crates/anie-config/src/lib.rs` — `SandboxToolConfig`,
  `ToolsConfig.sandbox`, partial-config struct + merge, default-config
  comment block (`DEFAULT_CONFIG_TEMPLATE` around `lib.rs:930`).
- `crates/anie-tools/Cargo.toml` — depend on `anie-sandbox`.
- `crates/anie-tools/src/bash.rs` — accept an `Option<SandboxSpec>`, call
  `anie_sandbox::apply` on the `Command` before `spawn` (`bash.rs:104-111`),
  map `SandboxError` → `ToolError`.
- `crates/anie-agent/src/tool.rs` — add `ToolError::SandboxSetup` variant
  (PR 5 commit 1).
- `crates/anie-cli/Cargo.toml` + `crates/anie-cli/src/bootstrap.rs` — build
  `SandboxSpec` from `config.tools.sandbox` + `cwd`, pass to
  `BashTool::with_policy(...)` (extend signature) (`bootstrap.rs:152-155`,
  `203-209`).
- `docs/arch/anie-rs_architecture.md` — new "Process sandbox" subsection
  near the filesystem-safety boundary (`:465-476`); update the "no
  sandboxing" framing (`:27-31`) to "off by default, opt-in on Linux".
- `docs/ROADMAP.md` — mark initiative #7 in progress / shipped.

Each PR below stays within the ≤5-files/PR rule.

---

## 4. Phased PRs

Cadence: one commit per PR, `cargo test --workspace` + `cargo clippy
--workspace --all-targets -- -D warnings` + `cargo fmt --check` green before
the next, manual smoke per `docs/smoke_protocol_2026-05-01.md`. Commit
style `sandbox/<PR#>: <imperative>` with a why-body and the `Co-Authored-By`
line.

### PR 1 — `sandbox/1`: crate skeleton + spec + no-op applier

Scope: create `anie-sandbox` with `SandboxSpec`, `SandboxError`, and an
`apply()` that is a **no-op for `None`** and returns
`SandboxError::Unsupported` for `Some` when the backend feature is off. Add
a `sandbox-linux` feature flag (no backend yet — empty stub). Register the
crate in the workspace. **Includes a documented `cargo tree -p
anie-sandbox` check** and a throwaway spike note confirming
`tokio::process::Command::pre_exec` is callable (recorded in this README's
revision log, not committed code).

Files (4): `crates/anie-sandbox/Cargo.toml`,
`crates/anie-sandbox/src/lib.rs`, root `Cargo.toml`,
`crates/anie-sandbox/src/linux.rs` (stub).

Tests:
- `spec_disabled_apply_leaves_command_unmodified`
- `spec_some_without_backend_feature_returns_unsupported`
- `writable_roots_default_spec_is_empty_not_root`

Exit: crate compiles on Linux/macOS; no behavior change anywhere else;
clippy/fmt green.

### PR 2 — `sandbox/2`: Linux Landlock filesystem confinement

Scope: implement `linux.rs` Landlock backend behind `sandbox-linux`:
build a ruleset granting read on `/`, read+write on each `writable_root`,
installed in the child via `pre_exec`. Parent-side kernel ABI detection;
`require_kernel_support` honored (fail-closed default).

Files (2): `crates/anie-sandbox/src/linux.rs`,
`crates/anie-sandbox/src/lib.rs`.

Tests (Linux, several gated behind a `landlock_available()` guard that
skips on kernels/CI without Landlock):
- `landlock_absent_with_require_support_returns_unsupported`
- `landlock_absent_without_require_support_runs_unconfined_ok`
- `write_outside_writable_root_is_denied` *(kernel-gated integration)*
- `write_inside_writable_root_succeeds` *(kernel-gated integration)*
- `read_outside_writable_root_still_succeeds` *(kernel-gated integration)*

Exit: with Landlock present, a child writing outside `writable_roots` fails
while reads succeed; with `require_kernel_support=true` and no Landlock,
spawn refuses with a typed error.

### PR 3 — `sandbox/3`: seccomp network confinement

Scope: extend the Linux backend so `allow_network=false` installs a seccomp
BPF filter denying socket-family syscalls (`socket`/`socketcall` as
applicable), combined with the Landlock ruleset in the same `pre_exec`.
`allow_network=true` installs no network filter.

Files (2): `crates/anie-sandbox/src/linux.rs`,
`crates/anie-sandbox/src/lib.rs`.

Tests (Linux, kernel/seccomp-gated):
- `network_denied_blocks_outbound_socket`
- `network_allowed_permits_outbound_socket`
- `seccomp_filter_does_not_block_benign_file_io`

Exit: a confined child with `allow_network=false` cannot open a socket; with
`true` it can; ordinary file I/O inside writable roots is unaffected.

### PR 4 — `sandbox/4`: `[tools.sandbox]` config

Scope: add `SandboxToolConfig` + `ToolsConfig.sandbox`, partial-config
struct + merge logic (mirroring `PartialBashToolConfig`,
`crates/anie-config/src/lib.rs:1141-1149`), and the commented default-config
template block. **No `anie-sandbox` dep needed in `anie-config`** — config
holds plain data; `anie-cli` translates it to a `SandboxSpec`.

Files (1): `crates/anie-config/src/lib.rs`.

Tests:
- `sandbox_config_defaults_to_disabled`
- `sandbox_config_roundtrips_through_toml`
- `sandbox_config_partial_merge_overrides_only_set_fields`
- `sandbox_writable_roots_default_is_empty`

Exit: config parses/merges; default is `enabled=false`; round-trip stable.

### PR 5 — `sandbox/5`: wire into bash spawn + bootstrap + docs

Scope (two commits, squashed-by-PR but logically ordered):
1. `anie-agent`: add `ToolError::SandboxSetup(String)` variant (separable
   1-variant refactor; thiserror message + matches updated).
2. `anie-tools` + `anie-cli`: `BashTool` takes an `Option<SandboxSpec>`;
   call `anie_sandbox::apply` on the `Command` before `spawn`
   (`bash.rs:104-111`); map `SandboxError → ToolError::SandboxSetup`; record
   sandbox provenance in result `details`. `bootstrap.rs` builds the spec
   from `config.tools.sandbox` + `cwd` (default roots `[cwd, tempdir]`) and
   passes it through (`bootstrap.rs:131-156`). Update arch doc + ROADMAP.

Files (5): `crates/anie-agent/src/tool.rs`,
`crates/anie-tools/src/bash.rs`, `crates/anie-cli/src/bootstrap.rs`,
`docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md`. (Cargo.toml dep edits
for `anie-tools`/`anie-cli` ride with these; if the file count exceeds 5,
split the Cargo.toml edits into a tiny `sandbox/5a` plumbing commit.)

Tests:
- `bash_with_sandbox_disabled_behaves_identically_to_today` *(regression)*
- `bash_sandbox_spec_built_from_config_uses_cwd_when_roots_empty`
- `bash_sandbox_setup_failure_surfaces_typed_sandbox_setup_error`
- `bash_sandboxed_write_outside_workspace_returns_error_not_panic`
  *(Linux, kernel-gated)*

Exit: with `[tools.sandbox] enabled=false` (default) behavior is unchanged;
with `enabled=true` on a Landlock kernel, `bash` writes are confined and
network is blocked by default; failures are typed; arch doc + ROADMAP
updated.

---

## 5. Test plan

Named tests, grouped by crate. (Kernel-dependent tests are guarded by a
`landlock_available()` / `seccomp_available()` helper that `return`s early on
unsupported CI, so the suite stays green everywhere.)

`anie-sandbox`:
- `spec_disabled_apply_leaves_command_unmodified`
- `spec_some_without_backend_feature_returns_unsupported`
- `writable_roots_default_spec_is_empty_not_root`
- `landlock_absent_with_require_support_returns_unsupported`
- `landlock_absent_without_require_support_runs_unconfined_ok`
- `write_outside_writable_root_is_denied`
- `write_inside_writable_root_succeeds`
- `read_outside_writable_root_still_succeeds`
- `network_denied_blocks_outbound_socket`
- `network_allowed_permits_outbound_socket`
- `seccomp_filter_does_not_block_benign_file_io`

`anie-config`:
- `sandbox_config_defaults_to_disabled`
- `sandbox_config_roundtrips_through_toml`
- `sandbox_config_partial_merge_overrides_only_set_fields`
- `sandbox_writable_roots_default_is_empty`

`anie-tools` / `anie-cli`:
- `bash_with_sandbox_disabled_behaves_identically_to_today`
- `bash_sandbox_spec_built_from_config_uses_cwd_when_roots_empty`
- `bash_sandbox_setup_failure_surfaces_typed_sandbox_setup_error`
- `bash_sandboxed_write_outside_workspace_returns_error_not_panic`

Manual smoke (per `docs/smoke_protocol_2026-05-01.md`): on a Landlock-capable
Linux box, run `anie` with `[tools.sandbox] enabled=true`, ask it to (a)
`cat` a file under cwd (allowed), (b) write to cwd (allowed), (c) write to
`/etc/anie-smoke` (denied → typed error reaches the model), (d) `curl`
example.com with `allow_network=false` (denied) then `true` (allowed).
Confirm `enabled=false` (default) is byte-identical to current behavior.

---

## 6. Risks

| Risk | Mitigation / punt |
|---|---|
| Landlock unevenly available (old kernels, some CI, containers). | `require_kernel_support=true` fail-closed default + capability detection; kernel-dependent tests gated and skip cleanly. Documented as Linux-only, opt-in. |
| `pre_exec` is `unsafe` and runs post-fork: the closure must be async-signal-safe and allocation-free. | Compile the ruleset/BPF in the **parent**; the closure only invokes `restrict_self`/`seccomp(apply)`. Documented inline. Spike in PR 1 confirms the pattern with `tokio::process::Command::pre_exec`. |
| Confining only the child (not anie) leaves anie itself unsandboxed. | **Intended** — anie is the trusted host; we confine untrusted tool commands. Called out as a deliberate deviation from a whole-process model (and from birdcage's re-exec model). |
| seccomp socket-deny is coarse vs Codex's network *proxy*. | Accepted: coarse on/off matches confirmed-finding scope; network proxying is explicitly deferred (§8). |
| Sandbox only wraps `bash`; `write`/`edit` tools still write unconfined. | Documented limitation. The Landlock confinement is process-scoped to the spawned shell; the in-process file tools are a separate hardening track (path-confinement) flagged in §8, not silently conflated (honors arch-doc "separate layer" directive). |
| New deps (`landlock`, `seccompiler`) add supply-chain surface. | Both are widely-used, focused crates; gated behind the `sandbox-linux` feature so default builds and non-Linux targets don't pull them. `cargo tree` check is a PR-1 step. |
| Bypass via interpreters still possible (e.g. a Python child that the shell execs). | Landlock/seccomp are **inherited across exec**, so descendants stay confined — strictly stronger than the textual `BashPolicy`. Documented. |

---

## 7. Exit criteria

- [x] `anie-sandbox` crate exists as a separate tool-execution layer;
      `anie-tools` only calls `apply`. (PR1)
- [x] `SandboxSpec` is minimal (writable-roots + network toggle +
      require_kernel_support); no `mode` matrix.
- [x] `[tools.sandbox]` defaults to `enabled=false`; disabled is
      byte-identical to today. (PR4/PR5 + regression test)
- [x] Setup failures surface as typed `ToolError::SandboxSetup`. (PR5)
- [x] On a Landlock kernel: writes confined to `writable_roots`, network
      off by default — **verified on this kernel** by real spawned-process
      integration tests (PR2/PR3) + the bash write-outside test (PR5).
- [x] **No** `CURRENT_SESSION_SCHEMA_VERSION` bump (config + freeform
      `details`) — stated in the PR-4/PR-5 commit bodies.
- [x] `cargo test --workspace`, `cargo clippy --workspace --all-targets`,
      `cargo fmt --check` all green (default + `sandbox-linux` feature).
- [x] `docs/arch/anie-rs_architecture.md` updated (sandbox subsection +
      revised framing). (PR5)
- [x] `docs/ROADMAP.md` updated. (PR5)
- [x] Approval-layer coupling recorded (§1a); escalation seam left open
      (the spec is per-invocation, not baked into `BashTool`).
- [~] Manual smoke on a live model: the OS-confinement behavior is verified
      by the gated integration tests; a model-driven end-to-end needs an
      API key (not run here).

---

## 8. Deferred (considered, explicitly NOT done)

- **Interactive escalation / approval UI** — "run unsandboxed once",
  per-command relax. Belongs to initiative #1 (approval layer). This plan
  only leaves the seam open (per-invocation `SandboxSpec`).
- **macOS seatbelt** and **Windows restricted tokens** — documented as the
  next platform targets; `apply()` returns `Unsupported` there for now.
- **`birdcage`-style whole-process / re-exec sandbox** — evaluated. Rejected
  as the primary because it confines the calling process, whereas we want to
  confine *only* the spawned tool child via `pre_exec`. Reconsider if/when we
  add macOS, where birdcage's unified Linux+macOS API may reduce backend
  code (noted in deps `why`).
- **Network *proxying*** (Codex-style filtered egress) — out of scope;
  coarse on/off only.
- **Confining the in-process `write`/`edit`/`read` tools** — these run in
  anie's own address space, so OS process-sandboxing can't wrap them without
  confining anie itself. A separate path-confinement track (the arch doc's
  "don't quietly change the path resolver" caveat applies) — not here.
- **A `ReadOnly` / `DangerFullAccess` mode enum** — additive later; the
  single workspace-write profile covers the v1 need.
- **Typed persisted sandbox provenance field** on session tool entries —
  would require a schema bump; freeform `details` suffices for v1.
- **WASM tool execution** — the arch doc's other stated long-term direction;
  orthogonal to OS process sandboxing and out of scope.

---

## Dependencies to add (gated behind `sandbox-linux`)

| Crate | Line | Why |
|---|---|---|
| `landlock` | `landlock = "0.4"` | Official Rust binding for the Landlock LSM; builds the filesystem ruleset and `restrict_self()` in the child `pre_exec`. Confirm latest at impl time. |
| `seccompiler` | `seccompiler = "0.4"` | Firecracker's seccomp BPF compiler; compiles the network-deny filter in the parent, applies in the child. Confirm latest at impl time. |

`nix` (already a workspace dep, `Cargo.toml:50`) is reused for any
fork/signal glue. **Alternative considered:** `birdcage` (single crate,
Linux Landlock+seccomp *and* macOS seatbelt) — deferred (§8) because it uses
a whole-process model; revisit when macOS lands.
