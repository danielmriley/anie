//! [`McpTool`]: adapts one MCP server tool to anie's [`Tool`] trait so
//! the agent loop drives it identically to a built-in. The tool name
//! is namespaced `mcp__<server>__<tool>` to avoid collisions; the bare
//! server-side name is what we send back on `tools/call`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use anie_agent::{Tool, ToolError, ToolExecutionContext};
use anie_protocol::{ContentBlock, ToolDef, ToolResult};

use crate::client::McpClient;
use crate::protocol::{CallToolResult, McpContent, McpToolSpec};

/// Build the namespaced tool name exposed to the model.
#[must_use]
pub fn namespaced_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// One MCP server tool, presented to the agent as a [`Tool`].
pub struct McpTool {
    client: Arc<McpClient>,
    /// Server name (for namespacing and result provenance).
    server: String,
    /// Bare server-side tool name sent on `tools/call`.
    tool_name: String,
    /// Cached definition with the namespaced name and the server's
    /// input schema, compiled once.
    definition: ToolDef,
}

impl McpTool {
    /// Adapt `spec` (from `tools/list`) on `server` into a [`Tool`].
    #[must_use]
    pub fn new(client: Arc<McpClient>, server: &str, spec: &McpToolSpec) -> Self {
        let definition = ToolDef {
            name: namespaced_name(server, &spec.name),
            description: spec.description.clone().unwrap_or_default(),
            parameters: spec.input_schema.clone(),
        };
        Self {
            client,
            server: server.to_string(),
            tool_name: spec.name.clone(),
            definition,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDef {
        self.definition.clone()
    }

    async fn execute(
        &self,
        _call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _update_tx: Option<mpsc::Sender<ToolResult>>,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        // Send the BARE tool name — the namespace prefix is anie's,
        // not the server's. Race the RPC against cancellation so an
        // aborted run doesn't wait on a slow server.
        let call = self.client.call_tool(&self.tool_name, args);
        tokio::select! {
            () = cancel.cancelled() => Err(ToolError::Aborted),
            result = call => match result {
                Ok(res) => map_result(res, &self.server),
                Err(err) => Err(ToolError::ExecutionFailed(format!(
                    "MCP server `{}`: {err}",
                    self.server
                ))),
            }
        }
    }
}

/// Map a successful `tools/call` result into a [`ToolResult`], or route
/// a server-flagged error through [`ToolError::ExecutionFailed`].
fn map_result(res: CallToolResult, server: &str) -> Result<ToolResult, ToolError> {
    let content: Vec<ContentBlock> = res.content.iter().map(map_content).collect();
    if res.is_error {
        let text = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let message = if text.is_empty() {
            format!("MCP tool on `{server}` reported an error")
        } else {
            text
        };
        return Err(ToolError::ExecutionFailed(message));
    }
    Ok(ToolResult {
        content,
        details: json!({ "mcp_server": server }),
    })
}

fn map_content(content: &McpContent) -> ContentBlock {
    match content {
        McpContent::Text { text } => ContentBlock::Text { text: text.clone() },
        McpContent::Image { data, mime_type } => ContentBlock::Image {
            media_type: mime_type.clone(),
            data: data.clone(),
        },
        // Full resources protocol is deferred; flatten an embedded
        // resource to a text marker so the model still sees something.
        McpContent::Resource { resource } => {
            let text = match &resource.text {
                Some(body) if !body.is_empty() => {
                    format!("[resource: {}]\n{body}", resource.uri)
                }
                _ => format!("[resource: {}]", resource.uri),
            };
            ContentBlock::Text { text }
        }
        McpContent::Unknown => ContentBlock::Text {
            text: "[unsupported MCP content block]".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Mock MCP server. `tools/call` branches on the (bare) tool name:
    ///   - `image_tool` -> image content
    ///   - `err_tool`   -> isError with text "boom"
    ///   - otherwise    -> a text block echoing the name the server
    ///     actually received (proves the namespace prefix was stripped)
    const MOCK: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | grep -o '[0-9]*')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"0\"}}}" ;;
    *notifications/initialized*) : ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[]}}" ;;
    *'"method":"tools/call"'*)
      name=$(printf '%s' "$line" | grep -o '"name":"[^"]*"' | head -1 | sed 's/.*"name":"//; s/"$//')
      case "$line" in
        *'"name":"image_tool"'*) result='{"content":[{"type":"image","data":"QUJD","mimeType":"image/png"}],"isError":false}' ;;
        *'"name":"err_tool"'*)   result='{"content":[{"type":"text","text":"boom"}],"isError":true}' ;;
        *) result="{\"content\":[{\"type\":\"text\",\"text\":\"$name\"}],\"isError\":false}" ;;
      esac
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":$result}" ;;
  esac
done
"#;

    async fn connected_client() -> Arc<McpClient> {
        let client = McpClient::connect(
            "bash",
            &["-c".to_string(), MOCK.to_string()],
            &HashMap::new(),
            Duration::from_secs(5),
        )
        .await
        .expect("connect to mock");
        Arc::new(client)
    }

    fn spec(name: &str) -> McpToolSpec {
        McpToolSpec {
            name: name.to_string(),
            description: Some(format!("{name} description")),
            input_schema: json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } }
            }),
        }
    }

    #[tokio::test]
    async fn mcp_tool_definition_namespaces_name_and_preserves_input_schema() {
        let client = connected_client().await;
        let tool = McpTool::new(client, "github", &spec("echo"));
        let def = tool.definition();
        assert_eq!(def.name, "mcp__github__echo");
        assert_eq!(def.description, "echo description");
        // Schema is passed through verbatim for the registry to compile.
        assert_eq!(def.parameters["properties"]["msg"]["type"], json!("string"));
    }

    async fn run(tool: &McpTool, args: Value) -> Result<ToolResult, ToolError> {
        tool.execute(
            "call-1",
            args,
            CancellationToken::new(),
            None,
            &ToolExecutionContext::default(),
        )
        .await
    }

    #[tokio::test]
    async fn mcp_tool_execute_maps_text_content_to_text_block() {
        let client = connected_client().await;
        let tool = McpTool::new(client, "srv", &spec("echo"));
        let result = run(&tool, json!({})).await.expect("ok");
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "echo"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_tool_execute_maps_image_content_to_image_block() {
        let client = connected_client().await;
        let tool = McpTool::new(client, "srv", &spec("image_tool"));
        let result = run(&tool, json!({})).await.expect("ok");
        match &result.content[0] {
            ContentBlock::Image { media_type, data } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data, "QUJD");
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_tool_execute_strips_namespace_prefix_before_calling_server() {
        // The model calls `mcp__srv__echo`, but the server must receive
        // the bare `echo`. The mock echoes the name it saw.
        let client = connected_client().await;
        let tool = McpTool::new(client, "srv", &spec("echo"));
        assert_eq!(tool.definition().name, "mcp__srv__echo");
        let result = run(&tool, json!({})).await.expect("ok");
        match &result.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "echo", "server saw the bare name"),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mcp_tool_execute_returns_execution_failed_on_is_error() {
        let client = connected_client().await;
        let tool = McpTool::new(client, "srv", &spec("err_tool"));
        let err = run(&tool, json!({})).await.expect_err("must be an error");
        assert_eq!(err, ToolError::ExecutionFailed("boom".to_string()));
    }

    #[tokio::test]
    async fn mcp_tool_execute_returns_execution_failed_on_transport_death() {
        // A server that completes the handshake but exits the moment it
        // receives a tools/call. The in-flight RPC sees the transport
        // close and must map to ExecutionFailed, never a panic.
        const DEAD_ON_CALL: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | grep -o '[0-9]*')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{}}}" ;;
    *notifications/initialized*) : ;;
    *'"method":"tools/call"'*) exit 0 ;;
  esac
done
"#;
        let dead_client = McpClient::connect(
            "bash",
            &["-c".to_string(), DEAD_ON_CALL.to_string()],
            &HashMap::new(),
            Duration::from_secs(5),
        )
        .await
        .expect("handshake completes before the server dies on call");
        let tool = McpTool::new(Arc::new(dead_client), "srv", &spec("echo"));
        let err = run(&tool, json!({}))
            .await
            .expect_err("call on a dying server must fail");
        assert!(matches!(err, ToolError::ExecutionFailed(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn mcp_tool_records_server_name_in_result_details() {
        let client = connected_client().await;
        let tool = McpTool::new(client, "myserver", &spec("echo"));
        let result = run(&tool, json!({})).await.expect("ok");
        assert_eq!(result.details["mcp_server"], json!("myserver"));
    }
}
