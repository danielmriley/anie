# MCP client: plan for anie

anie has zero MCP today. This plan adds an **MCP client** that, at
bootstrap, spawns configured external MCP servers over stdio,
discovers their tools, and registers each as a dynamic `Tool` impl
in the existing `ToolRegistry` — the same registry the agent loop
already drives. Scope is **client + tools only**. Resources,
prompts, SSE/HTTP transport, OAuth bridging, and exposing anie *as*
an MCP server are explicitly deferred (§8).

Grounding: `docs/rival_analysis_2026-06-06/findings_by_lens.json`
lens `mcp` (MCP-GAP-1, -2, -6 are the confirmed in-scope gaps).
Ranked initiative #2 on the shortlist (`docs/rival_analysis_2026-06-06/README.md`).

> **pi reference unavailable.** The pi tree is not on this machine.
> No pi `file:line` is cited below; where a shape comes from a
> rival, it is the MCP wire spec or Claude Code's observable
> `mcp__<server>__<tool>` naming, not a verified pi source.

> **Rival baselines are partly speculative.** The findings note
> "Codex has `codex-mcp` / `rmcp-client` / `mcp-server`" — treated
> here as a hypothesis about scope, not a spec to match field for
> field. We match the MCP wire protocol, which is the real
> contract.

---

## 1. Rationale

### The gap (verified)

- **MCP-GAP-1** — "No MCP client implementation; tools are
  compile-time registered only." Verified: every tool is
  registered in `build_tool_registry_with_policy()`
  (`crates/anie-cli/src/bootstrap.rs:131-186`) via
  `tools.register(Arc::new(ReadTool::new(...)))` and friends, then
  the registry is `Arc::new(tools)` (`bootstrap.rs:185`) and
  immutable thereafter.
- **MCP-GAP-2** — `ToolDef` is a fixed 3-field struct
  (`crates/anie-protocol/src/tools.rs:7-14`: `name`, `description`,
  `parameters: serde_json::Value`). No runtime schema discovery.
- **MCP-GAP-6** — `ToolRegistry::register` takes `&mut self`
  (`crates/anie-agent/src/tool.rs:171`); after `Arc`-wrapping the
  registry cannot be mutated. Tools must be registered *before* the
  `Arc` is built.

### Why this is the right lever

The registry already abstracts tools behind a single trait
(`Tool`, `crates/anie-agent/src/tool.rs:108-122`). The agent loop
looks a tool up by name and calls `execute` with no knowledge of
where the tool came from (`crates/anie-agent/src/agent_loop.rs:1426`
`self.tool_registry.get(&tool_call.name)`, executed at
`agent_loop.rs:1504`). A tool whose `execute` proxies to an
external process over JSON-RPC is indistinguishable from a built-in
to every consumer. **No agent-loop, protocol, or session change is
required** — MCP tools ride the existing seam. This is why the
initiative is high-impact for moderate effort.

### Why it slots cleanly into bootstrap

`prepare_controller_state` is already `async`
(`bootstrap.rs:27`) and already does fallible network/IO work
(model catalog discovery, `bootstrap.rs:36`). MCP server spawn +
handshake + `tools/list` is more async IO in the same place,
*before* the `Arc::new(tools)` immutability boundary. The
registration model ("register at startup, immutable after") is a
perfect fit — we are not introducing runtime hot-loading (that
stays deferred, MCP-GAP-6's broader ask).

### Dependency decision: hand-rolled vs. `rmcp`

**Recommendation: hand-roll a minimal JSON-RPC-2.0-over-stdio
client in a new `anie-mcp` crate. Do not add `rmcp`.**

Rationale, against CLAUDE.md §4 (reuse deps before adding) and the
project's "small shapes" principle:

- **Scope is tiny.** Client-only, stdio-only, three RPCs
  (`initialize`, `tools/list`, `tools/call`) plus one notification
  (`notifications/initialized`). That is ~200-300 LOC of transport
  + request/response plumbing. We do not need resources, prompts,
  sampling, roots, progress, or SSE — which is most of what a full
  SDK carries.
- **Every dependency already exists in the workspace.** `tokio`
  (`features = ["full"]`, so `process` + `io` are present),
  `serde`, `serde_json`, `async-trait`, `thiserror`, `tracing` are
  all workspace deps (root `Cargo.toml:42-93`). The new crate adds
  **zero** new external crates.
- **`rmcp` cost.** The official Rust SDK pulls a wider tree
  (`schemars`, `tower`-style middleware, its own transport
  abstractions, macro layer) and tracks the full bidirectional
  protocol. Adopting it to call three methods inverts the
  cost/value ratio and couples our tool surface to an external
  crate's evolving API. CLAUDE.md §4: don't pull a library whose
  surface dwarfs the capability we need.
- **Typed errors stay ours.** A hand-rolled client maps transport
  and protocol failures into a local `McpError` (`thiserror`),
  consistent with the `ProviderError`/`ToolError` taxonomy
  (CLAUDE.md "typed errors only"). `rmcp`'s error types would be a
  foreign taxonomy to translate anyway.

**Reversibility note:** the public seam is `Arc<dyn Tool>` produced
by an `McpManager`. If we later need SSE/HTTP/OAuth (deferred §8)
and decide `rmcp` is worth it then, only `anie-mcp`'s internals
change; nothing downstream of `Arc<dyn Tool>` is affected. The
decision is cheap to revisit.

A **step in PR 2** is to run `cargo tree -p anie-mcp` and confirm
no new external crate entered the lock relative to the workspace
baseline.

---

## 2. Design

### 2.1 New crate `anie-mcp`

A new workspace member, sitting between `anie-protocol`/`anie-agent`
and `anie-cli`. It depends on `anie-protocol` (for `ToolDef`,
`ToolResult`, `ContentBlock`) and `anie-agent` (for the `Tool`
trait, `ToolError`, `ToolExecutionContext`) — the same two crates
`anie-tools` depends on (cf. `crates/anie-tools/src/ls.rs:16-17`).

Modules:

| Module | Responsibility |
|--------|----------------|
| `error.rs` | `McpError` (`thiserror`) — spawn, handshake, transport, protocol, timeout. |
| `transport.rs` | `StdioTransport`: spawn child via `tokio::process::Command`, frame newline-delimited JSON-RPC over stdin/stdout, correlate responses by id. |
| `protocol.rs` | Minimal serde types: `JsonRpcRequest`/`Response`, `InitializeParams`/`Result`, `ListToolsResult`, `McpToolSpec` (`name`, `description`, `inputSchema`), `CallToolResult` (`content`, `isError`), `McpContent`. |
| `client.rs` | `McpClient`: owns a `StdioTransport`, performs `initialize` → `notifications/initialized` handshake, then `list_tools()` and `call_tool(name, args)`. |
| `tool.rs` | `McpTool` — implements `anie_agent::Tool`, wraps `Arc<McpClient>` + the server-qualified tool name + a cached `ToolDef`. |
| `manager.rs` | `McpManager::spawn_all(&McpConfig)` → spawns each enabled server, handshakes, lists tools, returns `Vec<Arc<dyn Tool>>` plus per-server status; **never fails the whole bootstrap on one dead server**. |
| `lib.rs` | Re-exports `McpManager`, `McpError`. |

### 2.2 Config shape — `[mcp]` in `anie-config`

Add to `AnieConfig` (`crates/anie-config/src/lib.rs:31-61`),
mirroring the `#[serde(default)]` pattern already used for `ui`,
`tools`, `ollama` (`lib.rs:42-51`):

```rust
/// External MCP servers to spawn at startup. Empty by default;
/// MCP is opt-in via config.
#[serde(default)]
pub mcp: McpConfig,
```

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    /// Server name -> launch spec. Name namespaces the server's
    /// tools as `mcp__<name>__<tool>`.
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// Executable to launch (stdio transport).
    pub command: String,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Disable a server without deleting its entry.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Handshake/list timeout. Default keeps a hung server from
    /// stalling startup.
    #[serde(default = "default_mcp_startup_timeout_ms")]
    pub startup_timeout_ms: u64,
}
```

Shape rationale: `command` / `args` / `env` is the universal MCP
stdio launch triple (Claude Code's `claude_desktop_config.json`,
Codex config). `enabled` + `startup_timeout_ms` are the two
operational knobs a dead-server-must-not-break-startup policy
needs. No `transport` field yet — stdio is the only variant; we add
an enum only when SSE/HTTP lands (§8). This is the deliberately
small shape per CLAUDE.md §1.

`startup_timeout_ms` default: **10_000** (10s) — generous enough for
an `npx`-launched server's cold start, short enough that a hung
server doesn't wedge a session. `default_true`/`default_mcp_startup_timeout_ms`
are module-level fns like the existing `default_markdown_enabled`
(`lib.rs:198`).

`PartialAnieConfig` (`lib.rs:1070`) gains a matching
`#[serde(default)] mcp` and `merge_partial_config` (`lib.rs:940`)
merges it, following the existing per-section merge pattern.

Config template (`default_config_template`, `lib.rs:929`) gains a
commented `[mcp.servers.<name>]` example.

### 2.3 Tool name namespacing

MCP tool `foo` from server `github` registers as
`mcp__github__foo`. This matches Claude Code's observable scheme and
prevents collisions with built-ins (`read`, `bash`, …) and across
servers. The prefix is constructed in `McpManager`, stored in the
`McpTool`'s cached `ToolDef.name`, and stripped back to the bare
tool name when issuing `tools/call` to the server.

### 2.4 Schema mapping: MCP → `ToolDef`

`McpToolSpec.inputSchema` is already a JSON Schema object — the
exact type `ToolDef.parameters` holds (`serde_json::Value`,
`tools.rs:13`). Mapping is:

```
ToolDef {
    name:        format!("mcp__{server}__{tool}"),
    description: spec.description.unwrap_or_default(),
    parameters:  spec.input_schema,   // verbatim
}
```

The registry compiles `parameters` into a `jsonschema::Validator`
at `register()` time exactly as for built-ins
(`tool.rs:174-179`); a server that ships an invalid schema yields a
`ValidatorState::Invalid` and surfaces on first call — identical
semantics to a built-in with a bad schema. **No special-casing.**

### 2.5 Call mapping: `ToolCall` → `tools/call` → `ToolResult`

`McpTool::execute` (signature fixed by the trait,
`tool.rs:113-121`):

1. Strip the `mcp__<server>__` prefix to recover the server-side
   tool name.
2. `client.call_tool(bare_name, args)` → `CallToolResult`.
3. Map `CallToolResult.content` (`Vec<McpContent>`) → `Vec<ContentBlock>`:
   - MCP `text` → `ContentBlock::Text { text }` (`content.rs:9`).
   - MCP `image` (`data` base64, `mimeType`) →
     `ContentBlock::Image { media_type, data }` (`content.rs:12`).
   - MCP `resource`/embedded content → flattened to
     `ContentBlock::Text` with a short `[resource: <uri>]` marker
     (full resource protocol is deferred §8).
4. `isError == true` → return `Err(ToolError::ExecutionFailed(...))`
   carrying the server's text content, so the agent loop routes it
   through the existing tool-error path. `isError` false/absent →
   `Ok(ToolResult { content, details })` where `details` carries
   `{"mcp_server": "<name>"}` for observability.
5. Transport death mid-call → `ToolError::ExecutionFailed` with the
   `McpError` rendered (no panic; a crashed server degrades to a
   failing tool, not a crashed agent).

The `cancel: CancellationToken` and `update_tx` params are accepted;
v1 honours `cancel` by aborting the in-flight RPC wait but does not
stream partial updates (MCP progress notifications are deferred §8).

### 2.6 Lifecycle & graceful failure

`McpManager::spawn_all` is called from `prepare_controller_state`
after selection (`bootstrap.rs:76`) but before `Arc::new(tools)`
(`bootstrap.rs:185`, inside `build_tool_registry_with_policy`).
Either refactor `build_tool_registry_with_policy` to call
`spawn_all` internally with `config.mcp`, or call `spawn_all` in
`prepare_controller_state` and pass the resulting
`Vec<Arc<dyn Tool>>` into an updated
`build_tool_registry_with_policy` signature. Per server, in
sequence:

1. Spawn child (`tokio::process::Command`, stdin/stdout piped, env
   applied).
2. `initialize` + `notifications/initialized`, bounded by
   `startup_timeout_ms`.
3. `tools/list`, same timeout.
4. On any failure (spawn error, handshake timeout, protocol error):
   **log `warn!` and skip that server** — return its tools as empty,
   keep going. Exactly mirrors the existing web-tools degradation
   ("log and continue without web tools rather than refuse to
   start", `bootstrap.rs:164-180`).

`suppress_tools` (`bootstrap.rs:83`, `--no-tools`/baseline harness)
also suppresses MCP spawn — baseline mode stays tool-free.

The spawned children are owned by the `McpClient`s, which are held
alive by the `Arc`s inside the registered `McpTool`s (themselves
held by the `Arc<ToolRegistry>` in `ControllerState`). When the
controller drops, the clients drop, and `StdioTransport`'s `Drop`
kills the child (`Child::kill`/`start_kill`). No separate daemon
supervisor in v1.

### 2.7 Approval seam interaction (note only — not built here)

MCP tools flow through `tool_registry.get()` →
`before_tool_call_hook` (`agent_loop.rs:1466-1468`) → `execute`
(`agent_loop.rs:1504`) identically to built-ins. The
`BeforeToolCallHook` trait (`crates/anie-agent/src/hooks.rs:39-47`)
is `pub(crate)` and currently test-only (its sole impl is in tests;
the module is `#![cfg_attr(not(test), allow(dead_code))]`,
`hooks.rs:11`).

**Decision: MCP tools require no special approval wiring in this
initiative.** Because they share the exact `Tool` seam, the future
permission/approval layer (shortlist #1) will gate them for free.
We deliberately do **not** auto-approve or auto-deny MCP tools here;
when #1 lands, a sensible default is "external/MCP tools are
untrusted by default" — but that policy belongs to #1, not here.
This plan only records the seam so #1 can target it. No `hooks.rs`
change.

---

## 3. Files to touch

New crate `crates/anie-mcp/`:
- `Cargo.toml`
- `src/lib.rs`
- `src/error.rs`
- `src/protocol.rs`
- `src/transport.rs`
- `src/client.rs`
- `src/tool.rs`
- `src/manager.rs`

Workspace / config:
- `Cargo.toml` (root) — add `crates/anie-mcp` to `members` and an
  `anie-mcp = { path = ... }` internal dep entry.
- `crates/anie-config/src/lib.rs` — `McpConfig`, `McpServerConfig`,
  `AnieConfig.mcp`, `PartialAnieConfig.mcp`, `merge_partial_config`,
  `default_config_template`, default fns.

Integration:
- `crates/anie-cli/Cargo.toml` — add `anie-mcp` dep.
- `crates/anie-cli/src/bootstrap.rs` — spawn manager, thread
  discovered tools into `build_tool_registry_with_policy`.

Docs (PR 6):
- `docs/arch/anie-rs_architecture.md`
- `docs/ROADMAP.md`

---

## 4. Phased PRs

Each PR is one commit, ≤5 files, `cargo test --workspace` +
`cargo clippy --workspace --all-targets -- -D warnings` +
`cargo fmt --check` green before the next.

Commit style: `mcp/PR<n>: <imperative>` + why-body + `Co-Authored-By`.

### PR 1 — `[mcp]` config section

**Files (2):** `crates/anie-config/src/lib.rs`, and this README's
example block reflected into `default_config_template`.

`McpConfig`/`McpServerConfig` with the §2.2 shape; wire into
`AnieConfig`, `PartialAnieConfig`, `merge_partial_config`, template,
default fns.

**Tests:**
- `mcp_config_defaults_to_empty_servers`
- `mcp_server_config_parses_command_args_env_from_toml`
- `mcp_server_enabled_defaults_true`
- `mcp_startup_timeout_defaults_to_ten_seconds`
- `partial_mcp_section_merges_without_clobbering_other_sections`
- `config_template_mcp_example_parses_when_uncommented`

**Exit:** config round-trips; absent `[mcp]` yields empty map; no
other section regresses.

### PR 2 — `anie-mcp` crate: stdio transport + error taxonomy

**Files (5):** root `Cargo.toml`, `crates/anie-mcp/Cargo.toml`,
`src/lib.rs`, `src/error.rs`, `src/transport.rs`.

JSON-RPC-2.0 framing over a spawned child's stdin/stdout; id
correlation; `McpError`. No protocol semantics yet. **Run
`cargo tree -p anie-mcp`; confirm zero new external crates.**

**Tests** (drive `StdioTransport` against a tiny in-test echo
script — e.g. a `bash -c` reader/writer, no new dep):
- `transport_spawns_child_and_roundtrips_one_request`
- `transport_correlates_responses_to_request_ids_out_of_order`
- `transport_surfaces_spawn_failure_as_typed_error`
- `transport_kills_child_on_drop`
- `transport_times_out_when_child_never_responds`

**Exit:** transport sends/receives framed JSON-RPC; child reaped on
drop; failures are typed `McpError`; cargo-tree clean.

### PR 3 — MCP client: handshake + `tools/list` + `tools/call`

**Files (3):** `src/protocol.rs`, `src/client.rs`, `src/lib.rs`.

`initialize` → `notifications/initialized`; `list_tools()`;
`call_tool()`. Protocol serde types.

**Tests** (mock-server script that answers `initialize`/`tools/list`/
`tools/call`):
- `client_completes_initialize_handshake`
- `client_lists_tools_with_input_schemas`
- `client_calls_tool_and_parses_text_content`
- `client_calls_tool_and_parses_image_content`
- `client_maps_is_error_result_to_typed_error`
- `client_initialize_timeout_yields_handshake_error`

**Exit:** the three RPCs work against the mock; errors typed.

### PR 4 — `McpTool`: schema → `ToolDef`, call → `ToolResult`

**Files (2):** `src/tool.rs`, `src/lib.rs`.

`McpTool` implements `anie_agent::Tool`; name namespacing; §2.4/2.5
mapping; `cancel` aborts the wait.

**Tests:**
- `mcp_tool_definition_namespaces_name_and_preserves_input_schema`
- `mcp_tool_execute_maps_text_content_to_text_block`
- `mcp_tool_execute_maps_image_content_to_image_block`
- `mcp_tool_execute_strips_namespace_prefix_before_calling_server`
- `mcp_tool_execute_returns_execution_failed_on_is_error`
- `mcp_tool_execute_returns_execution_failed_on_transport_death`
- `mcp_tool_records_server_name_in_result_details`

**Exit:** an `McpTool` is registry-compatible; schema compiles in
`ToolRegistry::register`; mappings verified.

### PR 5 — `McpManager` + bootstrap wiring (graceful failure)

**Files (3):** `src/manager.rs`,
`crates/anie-cli/src/bootstrap.rs`, `crates/anie-cli/Cargo.toml`.
(`src/lib.rs` re-export already added; if it needs an edit this PR,
that is still ≤5.)

`spawn_all` spawns enabled servers, lists tools, returns
`Vec<Arc<dyn Tool>>` + status; bootstrap registers them before
`Arc::new(tools)`; one dead server logs and is skipped;
`suppress_tools` suppresses MCP.

**Tests:**
- `manager_spawns_configured_server_and_returns_its_tools`
- `manager_skips_dead_server_and_returns_remaining_tools`
- `manager_returns_empty_when_no_servers_configured`
- `manager_respects_enabled_false`
- `bootstrap_registers_mcp_tools_into_registry` (anie-cli)
- `bootstrap_continues_when_mcp_server_fails_to_spawn` (anie-cli)
- `bootstrap_suppresses_mcp_when_no_tools` (anie-cli)

**Exit:** registry contains `mcp__*` tools after bootstrap with a
live server; a dead server does not abort startup; `--no-tools`
yields no MCP tools.

### PR 6 — docs

**Files (2):** `docs/arch/anie-rs_architecture.md`, `docs/ROADMAP.md`.

Document the `anie-mcp` crate, the bootstrap registration point, the
client-only/stdio-only scope, the approval-seam note, and the
deferred set. Mark the initiative landed on the roadmap.

**Exit:** arch doc names the crate + data flow; roadmap updated.

---

## 5. Test plan (consolidated, behavior-named)

Config (PR 1): `mcp_config_defaults_to_empty_servers`,
`mcp_server_config_parses_command_args_env_from_toml`,
`mcp_server_enabled_defaults_true`,
`mcp_startup_timeout_defaults_to_ten_seconds`,
`partial_mcp_section_merges_without_clobbering_other_sections`,
`config_template_mcp_example_parses_when_uncommented`.

Transport (PR 2): `transport_spawns_child_and_roundtrips_one_request`,
`transport_correlates_responses_to_request_ids_out_of_order`,
`transport_surfaces_spawn_failure_as_typed_error`,
`transport_kills_child_on_drop`,
`transport_times_out_when_child_never_responds`.

Client (PR 3): `client_completes_initialize_handshake`,
`client_lists_tools_with_input_schemas`,
`client_calls_tool_and_parses_text_content`,
`client_calls_tool_and_parses_image_content`,
`client_maps_is_error_result_to_typed_error`,
`client_initialize_timeout_yields_handshake_error`.

Tool (PR 4):
`mcp_tool_definition_namespaces_name_and_preserves_input_schema`,
`mcp_tool_execute_maps_text_content_to_text_block`,
`mcp_tool_execute_maps_image_content_to_image_block`,
`mcp_tool_execute_strips_namespace_prefix_before_calling_server`,
`mcp_tool_execute_returns_execution_failed_on_is_error`,
`mcp_tool_execute_returns_execution_failed_on_transport_death`,
`mcp_tool_records_server_name_in_result_details`.

Manager/bootstrap (PR 5):
`manager_spawns_configured_server_and_returns_its_tools`,
`manager_skips_dead_server_and_returns_remaining_tools`,
`manager_returns_empty_when_no_servers_configured`,
`manager_respects_enabled_false`,
`bootstrap_registers_mcp_tools_into_registry`,
`bootstrap_continues_when_mcp_server_fails_to_spawn`,
`bootstrap_suppresses_mcp_when_no_tools`.

Mock servers are small in-test scripts launched by the transport (no
new dependency); no live network. Manual smoke (§7) covers a real
server.

---

## 6. Risks

- **Hung server stalls startup.** Mitigation: `startup_timeout_ms`
  (default 10s) bounds handshake + list; timeout → skip server,
  continue. Tested by `..._times_out_when_child_never_responds` and
  `bootstrap_continues_when_mcp_server_fails_to_spawn`.
- **Zombie child processes.** Mitigation: `StdioTransport::Drop`
  kills the child; ownership chain (registry → `McpTool` → `Arc<McpClient>`
  → transport) guarantees drop on controller teardown. Tested by
  `transport_kills_child_on_drop`. Punt: a crashed server mid-session
  is not respawned in v1 (deferred §8); its tools then fail per-call,
  which the agent surfaces as tool errors.
- **Malformed / hostile schema from a server.** Mitigation: reuse
  `ToolRegistry`'s existing `ValidatorState::Invalid` path
  (`tool.rs:174-179`) — bad schema surfaces on first call, identical
  to a built-in. No new failure mode.
- **Tool-name collisions across servers / with built-ins.**
  Mitigation: `mcp__<server>__<tool>` namespacing; server names are
  config map keys (unique by construction).
- **Slow `tools/call` blocking a turn.** v1 awaits the RPC like any
  tool; `cancel` aborts the wait. No global timeout on `tools/call`
  in v1 (only on startup). Punt: per-call timeout is a one-line
  follow-up via config if real servers misbehave.
- **MCP protocol-version drift.** Mitigation: send the
  `protocolVersion` we implement in `initialize`; on a server that
  rejects it, the handshake errors and the server is skipped
  (logged), not crashed. We do not negotiate down in v1.
- **Untrusted external tools run with no approval.** Mitigation:
  documented seam (§2.7); MCP tools pass through `before_tool_call`
  already, so shortlist #1 gates them when it lands. We don't ship a
  half-approval here.
- **`rmcp` later proves necessary** (SSE/OAuth). Mitigation: the
  `Arc<dyn Tool>` boundary isolates the swap; revisiting is local to
  `anie-mcp`.

---

## 7. Exit criteria

- [x] PRs 1-6 merged in order (`d2d1c03`..`fb0116b`); each PR's named tests pass.
- [x] `cargo test --workspace` green (36 ok blocks, 0 failures; 22 anie-mcp tests).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [x] `cargo fmt --check` clean.
- [x] `cargo tree -p anie-mcp` adds **zero** new external crates
      vs. the workspace baseline (Cargo.lock gained only the `anie-mcp`
      entry; all deps come from existing workspace crates).
- [x] A configured stdio MCP server's tools appear in the registry
      as `mcp__<server>__<tool>` and are callable end-to-end
      (`bootstrap_registers_mcp_tools_into_registry`; live smoke below).
- [x] A misconfigured / dead server logs `warn!` and does **not**
      abort startup; other servers and built-ins still load
      (`manager_skips_dead_server_*`, `bootstrap_continues_when_mcp_server_fails_to_spawn`).
- [x] `--no-tools` / baseline harness mode spawns no MCP servers
      (`bootstrap_suppresses_mcp_when_no_tools`).
- [x] **Manual smoke**: captured as an `#[ignore]`d live test
      (`crates/anie-mcp/tests/live_server_smoke.rs`) that drives the real
      `@modelcontextprotocol/server-everything` through `McpClient`.
      Verified locally: handshake OK, 13 tools listed, `echo` called and
      returned a content block. (The "drive the model to call one" leg
      needs a live provider/API key and was not run here.)
- [x] `docs/arch/anie-rs_architecture.md` documents `anie-mcp` and
      the bootstrap registration point.
- [x] `docs/ROADMAP.md` marks the MCP client landed.

> **Schema bump:** none. MCP config lives in TOML, not in the
> session schema; tool calls/results already persist via the
> existing `ContentBlock`/`ToolResult` types
> (`CURRENT_SESSION_SCHEMA_VERSION = 4`,
> `crates/anie-session/src/lib.rs:90`). No persisted type gains a
> field, so `CURRENT_SESSION_SCHEMA_VERSION` is untouched and no
> forward-compat session test is required. (A config forward-compat
> test — old config without `[mcp]` loads clean — is covered by
> `mcp_config_defaults_to_empty_servers` in PR 1.)

---

## 8. Deferred (considered, explicitly not done)

- **SSE / streamable-HTTP transport.** stdio first. Adding a
  `transport` enum to `McpServerConfig` and an `HttpTransport` is the
  natural extension; not in v1. (MCP-GAP-1 mentions HTTP; we ship the
  most common transport first.)
- **MCP resources protocol** (`resources/list`, `resources/read`).
  MCP-GAP-2. Embedded resource content in tool results is flattened
  to a text marker (§2.5); the standalone resources surface is
  deferred.
- **MCP prompts protocol** (`prompts/list`, `prompts/get`).
  MCP-GAP-3. System-prompt composition stays as-is.
- **OAuth credential bridging to MCP servers.** MCP-GAP-5. The
  `user:mcp_servers` scope (`crates/anie-auth/src/anthropic_oauth.rs:55`)
  stays unused by this initiative; env-var auth via `[mcp.servers.*.env]`
  covers token-in-env servers in the meantime.
- **Server-push notifications / subscriptions / progress.**
  MCP-GAP-4. v1 is request/response; `update_tx` streaming and MCP
  progress notifications are not wired.
- **Runtime hot-load / re-scan / `/mcp` management commands.**
  MCP-GAP-6's broader ask. Registration stays startup-only, matching
  the registry's immutable-after-bootstrap model (`tool.rs:171`).
- **Crashed-server respawn / supervision.** A server that dies
  mid-session has failing tools until restart; no auto-respawn in v1.
- **Per-call `tools/call` timeout.** Only startup is bounded in v1.
- **Exposing anie as an MCP server** (Codex's `mcp-server`).
  Out of scope by the brief; a separate initiative.
- **Tool permission/approval policy for MCP tools.** Belongs to
  shortlist #1 (`hooks.rs` `BeforeToolCallHook`); §2.7 only records
  the seam.
- **Adopting `rmcp`.** Considered (§1); deferred pending SSE/OAuth
  need.
