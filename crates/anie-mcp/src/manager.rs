//! [`McpManager`]: spawns every configured MCP server, discovers its
//! tools, and returns them ready to register — never failing the whole
//! batch because one server is dead (mirrors anie's web-tools "log and
//! continue" degradation).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anie_agent::Tool;

use crate::client::McpClient;
use crate::error::McpError;
use crate::tool::McpTool;

/// Launch spec for one server.
///
/// anie-specific (deviation from the plan's `spawn_all(&McpConfig)`):
/// the manager takes this owned shape instead of `anie_config::McpConfig`
/// so `anie-mcp` doesn't depend on `anie-config`, matching how
/// `anie-tools` stays decoupled from the config crate (the CLI threads
/// config values in). `anie-cli::bootstrap` builds these from `[mcp]`.
#[derive(Debug, Clone)]
pub struct McpServerLaunch {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
    pub startup_timeout: Duration,
}

/// Outcome of attempting to bring up one server.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    /// Number of tools registered (0 when disabled or failed).
    pub tool_count: usize,
    /// `false` when skipped via `enabled = false`.
    pub enabled: bool,
    /// `Some` with a rendered `McpError` when the server failed to come
    /// up; `None` on success or when disabled.
    pub error: Option<String>,
}

/// Spawns configured MCP servers and adapts their tools.
pub struct McpManager;

impl McpManager {
    /// Bring up every enabled server in `launches` (sequentially) and
    /// return all discovered tools plus a per-server status. A server
    /// that fails to spawn, handshake, or list is logged and skipped;
    /// the rest still load.
    pub async fn spawn_all(
        launches: &[McpServerLaunch],
    ) -> (Vec<Arc<dyn Tool>>, Vec<ServerStatus>) {
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut statuses = Vec::new();

        for launch in launches {
            if !launch.enabled {
                statuses.push(ServerStatus {
                    name: launch.name.clone(),
                    tool_count: 0,
                    enabled: false,
                    error: None,
                });
                continue;
            }

            match Self::spawn_one(launch).await {
                Ok(server_tools) => {
                    statuses.push(ServerStatus {
                        name: launch.name.clone(),
                        tool_count: server_tools.len(),
                        enabled: true,
                        error: None,
                    });
                    tools.extend(server_tools);
                }
                Err(err) => {
                    tracing::warn!(
                        server = %launch.name,
                        error = %err,
                        "MCP server failed to start; skipping it"
                    );
                    statuses.push(ServerStatus {
                        name: launch.name.clone(),
                        tool_count: 0,
                        enabled: true,
                        error: Some(err.to_string()),
                    });
                }
            }
        }

        (tools, statuses)
    }

    async fn spawn_one(launch: &McpServerLaunch) -> Result<Vec<Arc<dyn Tool>>, McpError> {
        let client = McpClient::connect(
            &launch.command,
            &launch.args,
            &launch.env,
            launch.startup_timeout,
        )
        .await?;
        let specs = client.list_tools().await?;
        let client = Arc::new(client);
        let tools = specs
            .iter()
            .map(|spec| {
                Arc::new(McpTool::new(Arc::clone(&client), &launch.name, spec)) as Arc<dyn Tool>
            })
            .collect();
        Ok(tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock that lists one tool named `echo` and stays alive reading
    /// stdin. `extra_tool` optionally adds a second tool so we can tell
    /// servers apart in the multi-server test.
    fn mock_listing(tool: &str) -> String {
        format!(
            r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | grep -o '[0-9]*')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{}}}}}}" ;;
    *notifications/initialized*) : ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{{\"tools\":[{{\"name\":\"{tool}\",\"inputSchema\":{{\"type\":\"object\"}}}}]}}}}" ;;
  esac
done
"#
        )
    }

    fn launch(name: &str, command: &str, args: Vec<String>, enabled: bool) -> McpServerLaunch {
        McpServerLaunch {
            name: name.to_string(),
            command: command.to_string(),
            args,
            env: HashMap::new(),
            enabled,
            startup_timeout: Duration::from_secs(5),
        }
    }

    fn bash(script: String) -> Vec<String> {
        vec!["-c".to_string(), script]
    }

    #[tokio::test]
    async fn manager_spawns_configured_server_and_returns_its_tools() {
        let launches = vec![launch("alpha", "bash", bash(mock_listing("echo")), true)];
        let (tools, statuses) = McpManager::spawn_all(&launches).await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "mcp__alpha__echo");
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].tool_count, 1);
        assert!(statuses[0].error.is_none());
    }

    #[tokio::test]
    async fn manager_skips_dead_server_and_returns_remaining_tools() {
        let launches = vec![
            launch("dead", "anie-mcp-no-such-binary-zzz", vec![], true),
            launch("alive", "bash", bash(mock_listing("ping")), true),
        ];
        let (tools, statuses) = McpManager::spawn_all(&launches).await;
        // The live server's tool survives; the dead one contributes none.
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].definition().name, "mcp__alive__ping");
        let dead = statuses
            .iter()
            .find(|s| s.name == "dead")
            .expect("dead status");
        assert!(dead.error.is_some());
        let alive = statuses
            .iter()
            .find(|s| s.name == "alive")
            .expect("alive status");
        assert_eq!(alive.tool_count, 1);
    }

    #[tokio::test]
    async fn manager_returns_empty_when_no_servers_configured() {
        let (tools, statuses) = McpManager::spawn_all(&[]).await;
        assert!(tools.is_empty());
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn manager_respects_enabled_false() {
        let launches = vec![launch("off", "bash", bash(mock_listing("echo")), false)];
        let (tools, statuses) = McpManager::spawn_all(&launches).await;
        assert!(tools.is_empty(), "disabled server must not be spawned");
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].enabled);
        assert!(statuses[0].error.is_none());
    }
}
