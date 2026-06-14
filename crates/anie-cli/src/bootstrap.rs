use std::{path::Path, sync::Arc};

use anyhow::{Context, Result};

use anie_auth::AuthResolver;
use anie_config::{CliOverrides, load_config};
use anie_provider::{ProviderRegistry, RequestOptionsResolver};
use anie_providers_builtin::register_builtin_providers;
use anie_session::SessionManager;
use anie_tools::{
    ApplyPatchTool, BashPolicy, BashTool, EditTool, FileMutationQueue, FindTool, GrepTool, LsTool,
    ReadTool, TodoList, TodoWriteTool, WriteTool,
};
use anie_tui::UiAction;
use tracing::{info, warn};

use crate::{
    Cli,
    controller::ControllerState,
    model_catalog::{build_model_catalog, resolve_initial_selection},
    retry_policy::RetryConfig,
    runtime::{ConfigState, SessionHandle, SystemPromptCache},
    runtime_state::load_runtime_state,
};
use anie_agent::ToolRegistry;

pub(crate) async fn prepare_controller_state(cli: &Cli) -> Result<ControllerState> {
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let config = load_config(CliOverrides::default())?;
    let runtime_state = load_runtime_state().unwrap_or_default();

    let mut provider_registry = ProviderRegistry::new();
    register_builtin_providers(&mut provider_registry);
    let provider_registry = Arc::new(provider_registry);

    let (model_catalog, local_models_available) = build_model_catalog(&config).await;

    let sessions_dir =
        anie_config::anie_sessions_dir().context("home directory is not available")?;
    std::fs::create_dir_all(&sessions_dir)
        .with_context(|| format!("failed to create {}", sessions_dir.display()))?;

    let session = if let Some(session_id) = &cli.resume {
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        SessionManager::open_session(&path).map_err(|err| {
            if err.chain().any(|cause| {
                matches!(
                    cause.downcast_ref::<anie_session::SessionError>(),
                    Some(anie_session::SessionError::AlreadyOpen(_))
                )
            }) {
                anyhow::anyhow!(
                    "Session {session_id} is already open in another anie process.\n\
                     \n\
                     Options:\n\
                     - Close the other anie session and try again.\n\
                     - Use `/fork` from within the other process to branch.\n\
                     - Start a new session by omitting --resume."
                )
            } else {
                err.context(format!("failed to open session {session_id}"))
            }
        })?
    } else {
        SessionManager::new_session(&sessions_dir, &cwd)?
    };
    let session_context = session.build_context();

    let selection = resolve_initial_selection(
        cli,
        &config,
        &runtime_state,
        &session_context,
        &model_catalog,
        local_models_available,
    )?;

    // Plan `docs/rlm_2026-04-29/07_evaluation_harness.md`:
    // baseline mode opts out of tools entirely (model-only
    // measurement floor). The mode is captured in
    // ControllerState below for the rest of the harness to
    // consult; here we use it to gate tool registration.
    let suppress_tools = cli.no_tools || !cli.harness_mode.registers_tools();
    // Spawn configured MCP servers and discover their tools before the
    // registry is built (the registry is immutable once `Arc`-wrapped).
    // Suppressed-tools / baseline mode stays MCP-free.
    let mcp_tools = spawn_mcp_tools(&config.mcp, suppress_tools).await;
    // The plan/todo list is owned here and shared with the `todo_write`
    // tool registered below, so the status bar and verifier read the same
    // state. (See ControllerState::todo_list.)
    let todo_list = Arc::new(std::sync::Mutex::new(TodoList::default()));
    let tool_registry = build_tool_registry_with_policy(
        &cwd,
        suppress_tools,
        bash_policy_from_config(&config.tools.bash.policy),
        config.tools.web.clone(),
        mcp_tools,
        Arc::clone(&todo_list),
        sandbox_spec_from_config(&config.tools.sandbox, &cwd),
    );
    let skill_registry = Arc::new(crate::skills::SkillRegistry::discover(&cwd));
    if !skill_registry.is_empty() {
        info!(skills = skill_registry.len(), "loaded skills from disk");
    }
    let active_skills: crate::skill_tool::ActiveSkills = Arc::new(std::sync::RwLock::new(
        std::collections::HashSet::new(),
    ));
    // PR 2 of `docs/skills_2026-05-02/`: register the `skill`
    // tool when the registry has at least one skill. Without
    // skills the tool would be advertised in the catalog with
    // nothing to load, which is just noise.
    let tool_registry = if skill_registry.is_empty() {
        tool_registry
    } else {
        let skill_tool: Arc<dyn anie_agent::Tool> = Arc::new(crate::skill_tool::SkillTool::new(
            Arc::clone(&skill_registry),
            Arc::clone(&active_skills),
        ));
        Arc::new(tool_registry.with_added([skill_tool]))
    };
    // Plan 02 PR 7 of `docs/local_model_augmentation/`: the
    // `repo_map` drill-down tool is registered only when the map
    // policy is active (mirrors SkillTool's conditional
    // registration above — an always-on entry would be catalog
    // noise in modes where no map is ever injected). The cache is
    // shared with the per-run `RepoMapPolicy` via ControllerState.
    let repo_map_cache = crate::repo_map::SharedRepoMap::default();
    let tool_registry = with_repo_map_tool(
        tool_registry,
        !suppress_tools && crate::repo_map::repo_map_enabled(cli.harness_mode),
        &cwd,
        &repo_map_cache,
    );
    let prompt_cache = SystemPromptCache::build(&cwd, &tool_registry, &skill_registry, &config)?;
    let request_options_resolver: Arc<dyn RequestOptionsResolver> =
        Arc::new(AuthResolver::new(cli.api_key.clone(), config.clone()));

    // Cost meter priced from the selected model; on resume, rebuild the
    // session total by summing usage over the persisted messages.
    let cost_meter = Arc::new(crate::cost_meter::CostMeter::new(
        selection.model.cost_per_million.clone(),
    ));
    let persisted: Vec<_> = session_context
        .messages
        .iter()
        .map(|entry| entry.message.clone())
        .collect();
    cost_meter.rebuild_session(&persisted);

    // Register a `/skill:<name>` command for each discovered skill so
    // skills are user-invocable as well as model-invocable (the `skill`
    // tool above). Both paths resolve bodies from the same registry.
    let command_registry =
        crate::commands::CommandRegistry::with_builtins_and_skills(&skill_registry);

    let mut state = ControllerState {
        config: ConfigState::new(
            config,
            runtime_state,
            selection.model,
            selection.thinking,
            cli.api_key.clone(),
        ),
        session: SessionHandle::from_manager(session, sessions_dir, cwd),
        model_catalog,
        provider_registry,
        tool_registry,
        skill_registry,
        active_skills,
        request_options_resolver,
        prompt_cache,
        retry_config: RetryConfig::default(),
        command_registry,
        compaction_stats: Arc::new(crate::compaction_stats::CompactionStatsAtomic::default()),
        harness_mode: cli.harness_mode,
        rlm_archived_messages: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        repo_map_cache,
        todo_list,
        cost_meter,
        require_edit: cli.require_edit,
    };
    state.apply_session_overrides();
    if let Err(error) = state.persist_runtime_state() {
        warn!(%error, "failed to persist runtime state during bootstrap");
    }
    Ok(state)
}

/// Conditionally add the `repo_map` drill-down tool. `enabled`
/// is the map-policy gate (`repo_map_enabled` + tool suppression);
/// disabled returns the registry untouched so non-rlm catalogs
/// stay byte-identical. The tool's fallback build budget uses the
/// Full-tier default — like `SystemPromptCache::build`, the tier
/// isn't resolved until the controller state exists, and the
/// policy (which knows the tier) populates the shared cache at
/// the first model turn before any tool call can land.
fn with_repo_map_tool(
    registry: Arc<ToolRegistry>,
    enabled: bool,
    cwd: &Path,
    cache: &crate::repo_map::SharedRepoMap,
) -> Arc<ToolRegistry> {
    if !enabled {
        return registry;
    }
    let tool: Arc<dyn anie_agent::Tool> = Arc::new(crate::repo_map::RepoMapTool::new(
        cwd.to_path_buf(),
        crate::repo_map::repo_map_token_budget(crate::controller::PromptTier::Full),
        Arc::clone(cache),
    ));
    Arc::new(registry.with_added([tool]))
}

#[cfg(test)]
pub(crate) fn build_tool_registry(cwd: &Path, no_tools: bool) -> Arc<ToolRegistry> {
    build_tool_registry_with_policy(
        cwd,
        no_tools,
        BashPolicy::default(),
        anie_config::WebToolConfig::default(),
        Vec::new(),
        Arc::new(std::sync::Mutex::new(TodoList::default())),
        None,
    )
}

/// Build a `SandboxSpec` from `[tools.sandbox]` + cwd. Returns `None`
/// when the sandbox is disabled (the common default), preserving today's
/// behavior. Empty `writable_roots` defaults to `[cwd, $TMPDIR]`.
fn sandbox_spec_from_config(
    sandbox: &anie_config::SandboxToolConfig,
    cwd: &Path,
) -> Option<anie_sandbox::SandboxSpec> {
    if !sandbox.enabled {
        return None;
    }
    let writable_roots = if sandbox.writable_roots.is_empty() {
        vec![cwd.to_path_buf(), std::env::temp_dir()]
    } else {
        sandbox.writable_roots.clone()
    };
    Some(anie_sandbox::SandboxSpec {
        writable_roots,
        allow_network: sandbox.allow_network,
        require_kernel_support: sandbox.require_kernel_support,
    })
}

/// Convert the `[mcp]` config into manager launch specs (sorted by name
/// for deterministic registration), spawn the servers, and return their
/// tools. Suppressed-tools mode yields none. A dead server is logged and
/// skipped — it never aborts startup.
async fn spawn_mcp_tools(
    mcp: &anie_config::McpConfig,
    suppress_tools: bool,
) -> Vec<Arc<dyn anie_agent::Tool>> {
    if suppress_tools {
        return Vec::new();
    }
    let mut launches: Vec<anie_mcp::McpServerLaunch> = mcp
        .servers
        .iter()
        .map(|(name, server)| anie_mcp::McpServerLaunch {
            name: name.clone(),
            command: server.command.clone(),
            args: server.args.clone(),
            env: server.env.clone(),
            enabled: server.enabled,
            startup_timeout: std::time::Duration::from_millis(server.startup_timeout_ms),
        })
        .collect();
    launches.sort_by(|a, b| a.name.cmp(&b.name));

    let (tools, statuses) = anie_mcp::McpManager::spawn_all(&launches).await;
    for status in &statuses {
        match &status.error {
            Some(error) => warn!(server = %status.name, %error, "MCP server skipped"),
            None if status.enabled => {
                tracing::info!(server = %status.name, tools = status.tool_count, "MCP server connected");
            }
            None => {}
        }
    }
    tools
}

fn build_tool_registry_with_policy(
    cwd: &Path,
    no_tools: bool,
    bash_policy: BashPolicy,
    web_config: anie_config::WebToolConfig,
    mcp_tools: Vec<Arc<dyn anie_agent::Tool>>,
    todo_list: Arc<std::sync::Mutex<TodoList>>,
    sandbox: Option<anie_sandbox::SandboxSpec>,
) -> Arc<ToolRegistry> {
    let mut tools = ToolRegistry::new();
    if no_tools {
        return Arc::new(tools);
    }

    let queue = Arc::new(FileMutationQueue::new());
    tools.register(Arc::new(ReadTool::new(cwd.to_path_buf())));
    // The plan/todo tool, available in every tool-enabled mode (absent in
    // the no-tools baseline by virtue of the early return above). Writes
    // the controller-owned shared list.
    tools.register(Arc::new(TodoWriteTool::new(todo_list)));
    tools.register(Arc::new(WriteTool::with_queue(
        cwd.to_path_buf(),
        Arc::clone(&queue),
    )));
    tools.register(Arc::new(EditTool::with_queue(
        cwd.to_path_buf(),
        Arc::clone(&queue),
    )));
    // apply_patch shares the same mutation queue as write/edit so they
    // serialize against each other on shared paths.
    tools.register(Arc::new(ApplyPatchTool::with_queue(
        cwd.to_path_buf(),
        Arc::clone(&queue),
    )));
    tools.register(Arc::new(BashTool::with_sandbox(
        cwd.to_path_buf(),
        bash_policy,
        sandbox,
    )));
    tools.register(Arc::new(GrepTool::new(cwd.to_path_buf())));
    tools.register(Arc::new(FindTool::new(cwd.to_path_buf())));
    tools.register(Arc::new(LsTool::new(cwd.to_path_buf())));

    // Web tools — optional via the `web` cargo feature so
    // lean builds can compile them out entirely. The
    // `web_tools_with_options()` factory may fail if the
    // reqwest client can't be built (e.g., no TLS roots); we
    // log and continue without web tools rather than refuse
    // to start. The `[tools.web]` config supplied by the
    // operator is converted to `FetchOptions` here. PR 4.3 of
    // `docs/code_review_2026-04-27/`.
    #[cfg(feature = "web")]
    {
        let opts = web_fetch_options_from_config(&web_config);
        match anie_tools_web::web_tools_with_options(opts) {
            Ok(web) => {
                for tool in web {
                    tools.register(tool);
                }
            }
            Err(error) => {
                warn!(%error, "failed to initialize web tools; continuing without them");
            }
        }
    }
    #[cfg(not(feature = "web"))]
    let _ = web_config;

    // MCP tools discovered from configured external servers. Registered
    // after built-ins/web so a server cannot shadow a core tool name;
    // collisions are already prevented by the `mcp__<server>__` prefix.
    for tool in mcp_tools {
        tools.register(tool);
    }

    Arc::new(tools)
}

#[cfg(feature = "web")]
fn web_fetch_options_from_config(
    web_config: &anie_config::WebToolConfig,
) -> anie_tools_web::read::fetch::FetchOptions {
    use std::time::Duration;
    anie_tools_web::read::fetch::FetchOptions {
        timeout: Duration::from_secs(web_config.request_timeout_secs),
        user_agent: anie_tools_web::read::fetch::DEFAULT_USER_AGENT.into(),
        max_bytes: web_config.max_page_bytes,
        max_redirects: web_config.max_redirects,
        allow_private_ips: web_config.allow_private_ips,
        headless_timeout_secs: web_config.headless_timeout_secs,
    }
}

fn bash_policy_from_config(config: &anie_config::BashPolicyConfig) -> BashPolicy {
    BashPolicy {
        enabled: config.enabled,
        deny_commands: config.deny_commands.clone(),
        deny_patterns: config.deny_patterns.clone(),
    }
}

pub(crate) fn spawn_shutdown_signal_forwarder(
    action_tx: tokio::sync::mpsc::UnboundedSender<UiAction>,
) {
    #[cfg(not(unix))]
    let _ = action_tx;

    #[cfg(unix)]
    {
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};

            let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut sighup) = signal(SignalKind::hangup()) else {
                return;
            };

            tokio::select! {
                _ = sigterm.recv() => {
                    let _ = action_tx.send(UiAction::Quit);
                }
                _ = sighup.recv() => {
                    let _ = action_tx.send(UiAction::Quit);
                }
            }
        });
    }
}

#[cfg(test)]
mod mcp_bootstrap_tests {
    use super::*;
    use anie_config::{McpConfig, McpServerConfig};
    use std::collections::HashMap;

    /// A bash MCP server that completes the handshake, lists one tool
    /// named `echo`, and stays alive reading stdin.
    const LIST_ECHO_MOCK: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | grep -o '[0-9]*')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{}}}" ;;
    *notifications/initialized*) : ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"inputSchema\":{\"type\":\"object\"}}]}}" ;;
  esac
done
"#;

    fn server(command: &str, args: Vec<String>) -> McpServerConfig {
        McpServerConfig {
            command: command.to_string(),
            args,
            env: HashMap::new(),
            enabled: true,
            startup_timeout_ms: 5_000,
        }
    }

    fn mcp_with(name: &str, server_cfg: McpServerConfig) -> McpConfig {
        let mut servers = HashMap::new();
        servers.insert(name.to_string(), server_cfg);
        McpConfig { servers }
    }

    fn registry_with(mcp_tools: Vec<Arc<dyn anie_agent::Tool>>) -> Arc<ToolRegistry> {
        let tmp = tempfile::tempdir().expect("tempdir");
        build_tool_registry_with_policy(
            tmp.path(),
            false,
            BashPolicy::default(),
            anie_config::WebToolConfig::default(),
            mcp_tools,
            Arc::new(std::sync::Mutex::new(anie_tools::TodoList::default())),
            None,
        )
    }

    #[tokio::test]
    async fn bootstrap_registers_mcp_tools_into_registry() {
        let mcp = mcp_with(
            "test",
            server("bash", vec!["-c".to_string(), LIST_ECHO_MOCK.to_string()]),
        );
        let mcp_tools = spawn_mcp_tools(&mcp, false).await;
        assert_eq!(mcp_tools.len(), 1);

        let registry = registry_with(mcp_tools);
        assert!(
            registry.get("mcp__test__echo").is_some(),
            "MCP tool registered under its namespaced name"
        );
        assert!(registry.get("read").is_some(), "built-ins still present");
    }

    #[tokio::test]
    async fn bootstrap_continues_when_mcp_server_fails_to_spawn() {
        let mcp = mcp_with("bad", server("anie-no-such-binary-xyz", vec![]));
        let mcp_tools = spawn_mcp_tools(&mcp, false).await;
        assert!(mcp_tools.is_empty(), "a dead server contributes no tools");

        // Startup still yields a working registry with built-ins.
        let registry = registry_with(mcp_tools);
        assert!(registry.get("read").is_some());
    }

    #[tokio::test]
    async fn bootstrap_suppresses_mcp_when_no_tools() {
        let mcp = mcp_with(
            "test",
            server("bash", vec!["-c".to_string(), LIST_ECHO_MOCK.to_string()]),
        );
        let mcp_tools = spawn_mcp_tools(&mcp, true).await;
        assert!(
            mcp_tools.is_empty(),
            "suppressed-tools / baseline mode spawns no MCP servers"
        );
    }
}

#[cfg(test)]
mod repo_map_bootstrap_tests {
    use super::*;

    /// Plan 02 PR 7: the drill-down tool rides the same gate as
    /// the map policy — when the policy is disabled the catalog
    /// must stay byte-identical to today (no orphan `repo_map`
    /// entry advertising a map that's never injected).
    #[test]
    fn tool_is_unregistered_when_map_policy_is_disabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = build_tool_registry(tmp.path(), false);
        let cache = crate::repo_map::SharedRepoMap::default();

        let disabled = with_repo_map_tool(Arc::clone(&base), false, tmp.path(), &cache);
        assert!(disabled.get("repo_map").is_none());
        assert_eq!(
            disabled.definitions().len(),
            base.definitions().len(),
            "disabled gate must leave the registry untouched"
        );

        let enabled = with_repo_map_tool(base, true, tmp.path(), &cache);
        assert!(enabled.get("repo_map").is_some());
    }
}

#[cfg(test)]
mod sandbox_bootstrap_tests {
    use super::*;

    #[test]
    fn bash_sandbox_spec_built_from_config_uses_cwd_when_roots_empty() {
        // Disabled => no spec (today's behavior).
        let disabled = anie_config::SandboxToolConfig::default();
        assert!(sandbox_spec_from_config(&disabled, Path::new("/work")).is_none());

        // Enabled with empty roots => [cwd, $TMPDIR].
        let enabled = anie_config::SandboxToolConfig {
            enabled: true,
            ..anie_config::SandboxToolConfig::default()
        };
        let spec = sandbox_spec_from_config(&enabled, Path::new("/work")).expect("spec");
        assert!(
            spec.writable_roots
                .contains(&Path::new("/work").to_path_buf())
        );
        assert!(spec.writable_roots.contains(&std::env::temp_dir()));
        assert!(!spec.allow_network);
        assert!(spec.require_kernel_support);
    }

    #[test]
    fn bash_sandbox_spec_uses_explicit_roots_when_provided() {
        let cfg = anie_config::SandboxToolConfig {
            enabled: true,
            writable_roots: vec![std::path::PathBuf::from("/explicit")],
            allow_network: true,
            require_kernel_support: false,
        };
        let spec = sandbox_spec_from_config(&cfg, Path::new("/work")).expect("spec");
        assert_eq!(
            spec.writable_roots,
            vec![std::path::PathBuf::from("/explicit")]
        );
        assert!(spec.allow_network);
        assert!(!spec.require_kernel_support);
    }
}

#[cfg(test)]
mod todo_bootstrap_tests {
    use super::*;
    use anie_tools::{TodoList, TodoStatus};

    fn registry_with(
        todo_list: Arc<std::sync::Mutex<TodoList>>,
        no_tools: bool,
    ) -> Arc<ToolRegistry> {
        let tmp = tempfile::tempdir().expect("tempdir");
        build_tool_registry_with_policy(
            tmp.path(),
            no_tools,
            BashPolicy::default(),
            anie_config::WebToolConfig::default(),
            Vec::new(),
            todo_list,
            None,
        )
    }

    #[tokio::test]
    async fn todo_tool_registered_for_default_mode() {
        let list = Arc::new(std::sync::Mutex::new(TodoList::default()));
        let registry = registry_with(Arc::clone(&list), false);
        assert!(
            registry.get("todo_write").is_some(),
            "todo_write must be registered in tool-enabled modes"
        );
        // ...and absent in the no-tools baseline.
        let baseline = registry_with(list, true);
        assert!(baseline.get("todo_write").is_none());
    }

    #[tokio::test]
    async fn todo_write_call_mutates_controller_owned_list() {
        // The tool registered in the registry and the controller-owned
        // Arc<Mutex<TodoList>> are the same state — one source of truth.
        let list = Arc::new(std::sync::Mutex::new(TodoList::default()));
        let registry = registry_with(Arc::clone(&list), false);
        let tool = registry.get("todo_write").expect("todo_write present");

        tool.execute(
            "call",
            serde_json::json!({ "todos": [
                { "content": "step one", "status": "in_progress" },
                { "content": "step two", "status": "pending" }
            ] }),
            tokio_util::sync::CancellationToken::new(),
            None,
            &anie_agent::ToolExecutionContext::default(),
        )
        .await
        .expect("todo_write executes");

        let guard = list.lock().expect("lock");
        assert_eq!(guard.items().len(), 2);
        assert_eq!(guard.items()[0].content, "step one");
        assert_eq!(guard.items()[0].status, TodoStatus::InProgress);
        assert_eq!(guard.counts(), (0, 2));
    }
}
