//! Live end-to-end smoke against a real MCP server, exercising the
//! actual `McpClient` (spawn -> initialize -> tools/list -> tools/call)
//! rather than an in-test bash mock.
//!
//! `#[ignore]`d because it requires network + `npx` to fetch the
//! reference server, so it never runs in the normal `cargo test`
//! gate. Run it deliberately:
//!
//! ```text
//! cargo test -p anie-mcp --test live_server_smoke -- --ignored --nocapture
//! ```
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashMap;
use std::time::Duration;

use anie_mcp::McpClient;

#[tokio::test]
#[ignore = "requires network + npx to launch @modelcontextprotocol/server-everything"]
async fn live_everything_server_handshakes_lists_and_calls_echo() {
    let client = McpClient::connect(
        "npx",
        &[
            "-y".to_string(),
            "@modelcontextprotocol/server-everything".to_string(),
        ],
        &HashMap::new(),
        // npm cold-start can be slow on a fresh cache.
        Duration::from_secs(90),
    )
    .await
    .expect("connect to the real everything-server");

    let tools = client.list_tools().await.expect("list tools");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "everything-server advertises an `echo` tool; got {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let result = client
        .call_tool(
            "echo",
            serde_json::json!({ "message": "anie-mcp live smoke" }),
        )
        .await
        .expect("call echo");
    assert!(!result.is_error, "echo should succeed");
    assert!(
        !result.content.is_empty(),
        "echo returns at least one content block"
    );
    println!(
        "live smoke OK: {} tools, echo returned {} content block(s)",
        tools.len(),
        result.content.len()
    );
}
