//! High-level MCP client: completes the `initialize` handshake, then
//! exposes `list_tools` and `call_tool` over a [`StdioTransport`].

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::McpError;
use crate::protocol::{CallToolResult, ListToolsResult, McpToolSpec, PROTOCOL_VERSION};
use crate::transport::StdioTransport;

/// anie-specific backstop: v1 has no configurable per-call timeout
/// (plan §6 Deferred — real cancellation is the `cancel` token in
/// `McpTool`). A 1-hour ceiling prevents a permanently stuck waiter if
/// a server hangs while no user abort arrives.
const CALL_TOOL_TIMEOUT: Duration = Duration::from_secs(3600);

/// A connected MCP server: transport plus the startup timeout used for
/// the handshake and `tools/list`.
pub struct McpClient {
    transport: StdioTransport,
    startup_timeout: Duration,
}

impl McpClient {
    /// Spawn the server and complete the `initialize` /
    /// `notifications/initialized` handshake, bounded by
    /// `startup_timeout`. Handshake failures (spawn, timeout, protocol)
    /// surface as [`McpError::Handshake`] so the manager can skip the
    /// server uniformly.
    pub async fn connect(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        startup_timeout: Duration,
    ) -> Result<Self, McpError> {
        let transport = StdioTransport::spawn(command, args, env)?;
        let client = Self {
            transport,
            startup_timeout,
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "anie", "version": env!("CARGO_PKG_VERSION") },
        });
        // Everything in the handshake path reports as Handshake so a
        // dead/incompatible server is one error kind to the manager.
        self.transport
            .request("initialize", params, self.startup_timeout)
            .await
            .and_then(|resp| extract_result(resp, "initialize"))
            .map_err(|err| McpError::Handshake(err.to_string()))?;
        self.transport
            .notify("notifications/initialized", Value::Null)
            .await
            .map_err(|err| McpError::Handshake(err.to_string()))?;
        Ok(())
    }

    /// List the server's tools.
    pub async fn list_tools(&self) -> Result<Vec<McpToolSpec>, McpError> {
        let resp = self
            .transport
            .request("tools/list", json!({}), self.startup_timeout)
            .await?;
        let result = extract_result(resp, "tools/list")?;
        let parsed: ListToolsResult = serde_json::from_value(result)?;
        Ok(parsed.tools)
    }

    /// Call a tool by its server-side name with JSON arguments.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let resp = self
            .transport
            .request("tools/call", params, CALL_TOOL_TIMEOUT)
            .await?;
        let result = extract_result(resp, "tools/call")?;
        let parsed: CallToolResult = serde_json::from_value(result)?;
        Ok(parsed)
    }

    /// The server's OS process id, if running (for logging).
    #[must_use]
    pub fn child_id(&self) -> Option<u32> {
        self.transport.child_id()
    }
}

/// Pull the `result` out of a JSON-RPC response envelope, mapping a
/// JSON-RPC `error` object or a missing `result` to a typed
/// [`McpError::Protocol`].
fn extract_result(mut resp: Value, method: &str) -> Result<Value, McpError> {
    if let Some(error) = resp.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(McpError::Protocol(format!("{method} failed: {message}")));
    }
    match resp.get_mut("result") {
        Some(result) => Ok(result.take()),
        None => Err(McpError::Protocol(format!(
            "{method} response missing `result`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::McpContent;

    /// A bash MCP mock that answers `initialize`, `tools/list`, and
    /// `tools/call`. The call response varies by the requested tool
    /// name so one mock serves every call test:
    ///   - `image_tool` -> an image content block
    ///   - `err_tool`   -> `isError: true`
    ///   - anything else -> a text content block "hello"
    const MOCK: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | grep -o '[0-9]*')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{},\"serverInfo\":{\"name\":\"mock\",\"version\":\"0\"}}}" ;;
    *notifications/initialized*) : ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echo a message\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"msg\":{\"type\":\"string\"}}}}]}}" ;;
    *'"method":"tools/call"'*)
      case "$line" in
        *'"name":"image_tool"'*) result='{"content":[{"type":"image","data":"QUJD","mimeType":"image/png"}],"isError":false}' ;;
        *'"name":"err_tool"'*)   result='{"content":[{"type":"text","text":"boom"}],"isError":true}' ;;
        *)                       result='{"content":[{"type":"text","text":"hello"}],"isError":false}' ;;
      esac
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":$result}" ;;
  esac
done
"#;

    async fn connect_mock() -> McpClient {
        McpClient::connect(
            "bash",
            &["-c".to_string(), MOCK.to_string()],
            &HashMap::new(),
            Duration::from_secs(5),
        )
        .await
        .expect("connect to mock")
    }

    #[tokio::test]
    async fn client_completes_initialize_handshake() {
        // connect() returning Ok means initialize + initialized
        // notification both succeeded.
        let client = connect_mock().await;
        assert!(client.child_id().is_some());
    }

    #[tokio::test]
    async fn client_lists_tools_with_input_schemas() {
        let client = connect_mock().await;
        let tools = client.list_tools().await.expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(tools[0].description.as_deref(), Some("Echo a message"));
        assert_eq!(tools[0].input_schema["type"], json!("object"));
        assert_eq!(
            tools[0].input_schema["properties"]["msg"]["type"],
            json!("string")
        );
    }

    #[tokio::test]
    async fn client_calls_tool_and_parses_text_content() {
        let client = connect_mock().await;
        let result = client
            .call_tool("echo", json!({ "msg": "hi" }))
            .await
            .expect("call tool");
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            McpContent::Text { text } => assert_eq!(text, "hello"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_calls_tool_and_parses_image_content() {
        let client = connect_mock().await;
        let result = client
            .call_tool("image_tool", json!({}))
            .await
            .expect("call tool");
        assert!(!result.is_error);
        match &result.content[0] {
            McpContent::Image { data, mime_type } => {
                assert_eq!(data, "QUJD");
                assert_eq!(mime_type, "image/png");
            }
            other => panic!("expected image content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_maps_is_error_result_to_typed_error() {
        // tools/call with isError:true is NOT a transport error — it's
        // a successful RPC whose result flags a tool-level failure. The
        // client surfaces it as CallToolResult.is_error; mapping to
        // ToolError happens in McpTool (PR4).
        let client = connect_mock().await;
        let result = client
            .call_tool("err_tool", json!({}))
            .await
            .expect("rpc ok");
        assert!(result.is_error);
        match &result.content[0] {
            McpContent::Text { text } => assert_eq!(text, "boom"),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_initialize_timeout_yields_handshake_error() {
        // A server that never answers initialize must fail connect with
        // a Handshake error within the startup timeout.
        let result = McpClient::connect(
            "sleep",
            &["30".to_string()],
            &HashMap::new(),
            Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(result, Err(McpError::Handshake(_))),
            "expected a handshake error on initialize timeout"
        );
    }
}
