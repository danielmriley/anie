//! Newline-delimited JSON-RPC-2.0 transport over a spawned child's
//! stdio.
//!
//! MCP's stdio transport frames each JSON-RPC message as one line of
//! JSON on stdin/stdout. [`StdioTransport`] spawns the server, writes
//! requests, and a background reader task routes each response back to
//! the waiting caller by JSON-RPC `id`. Server-initiated requests and
//! notifications (no matching pending id) are ignored in v1.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::error::McpError;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

/// A live connection to one stdio MCP server.
///
/// Owns the child process; dropping the transport aborts the reader
/// task and kills the child (also guaranteed by `kill_on_drop`).
pub struct StdioTransport {
    child: Child,
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicU64,
    /// Set once the reader task exits (EOF or read error). New requests
    /// fail fast with `Closed` instead of blocking until their timeout.
    closed: Arc<AtomicBool>,
    reader: JoinHandle<()>,
}

impl StdioTransport {
    /// Spawn `command args` with `env` overlaid, pipe stdio, and start
    /// the background reader. Returns [`McpError::Spawn`] if the
    /// process cannot be launched.
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self, McpError> {
        let mut child = Command::new(command)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| McpError::Spawn {
                command: command.to_string(),
                source,
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("child stdin was not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("child stdout was not piped".to_string()))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let reader_pending = Arc::clone(&pending);
        let reader_closed = Arc::clone(&closed);
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<Value>(&line) else {
                            tracing::warn!(line = %line, "MCP: dropping non-JSON line from server");
                            continue;
                        };
                        // Only a *response* (no `method`) routes to a
                        // waiter. A server-initiated request/notification
                        // carries `method` and lives in the server's own
                        // id-space — its id can collide with an in-flight
                        // client id, so it must not be consumed as a
                        // response. Such messages are ignored in v1.
                        if value.get("method").is_none()
                            && let Some(id) = value.get("id").and_then(Value::as_u64)
                            && let Some(tx) = reader_pending.lock().await.remove(&id)
                        {
                            let _ = tx.send(value);
                        }
                    }
                    Ok(None) => break, // EOF: server closed stdout.
                    Err(error) => {
                        tracing::warn!(%error, "MCP: reader stopped on stdout read error");
                        break;
                    }
                }
            }
            // Reader is gone: mark closed so new requests fail fast, and
            // drop every in-flight waiter (their receivers resolve to
            // Closed) rather than leaving them to wait out the timeout.
            reader_closed.store(true, Ordering::Release);
            reader_pending.lock().await.clear();
        });

        Ok(Self {
            child,
            stdin: Mutex::new(stdin),
            pending,
            next_id: AtomicU64::new(1),
            closed,
            reader,
        })
    }

    /// The OS process id of the server, if still running. Useful for
    /// logging and for tests that assert teardown behavior.
    #[must_use]
    pub fn child_id(&self) -> Option<u32> {
        self.child.id()
    }

    /// Send a JSON-RPC request and await its response envelope,
    /// bounded by `timeout`. The returned [`Value`] is the full
    /// response object (the caller extracts `result`/`error`).
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        // Fast-fail once the reader has stopped: otherwise this request
        // would write to a dead server and block on its waiter until the
        // (up to 1-hour) timeout elapses.
        if self.closed.load(Ordering::Acquire) {
            return Err(McpError::Closed);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // Re-check after inserting: the reader may have closed (and
        // cleared `pending`) between the check and the insert, which
        // would leave this entry unresolved.
        if self.closed.load(Ordering::Acquire) {
            self.pending.lock().await.remove(&id);
            return Err(McpError::Closed);
        }

        let payload = envelope(Some(id), method, params);
        if let Err(err) = self.write_line(&payload).await {
            self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => Ok(value),
            // Sender dropped without sending: server closed stdout.
            Ok(Err(_)) => Err(McpError::Closed),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(McpError::Timeout {
                    method: method.to_string(),
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            }
        }
    }

    /// Send a JSON-RPC notification (no id, no response).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let payload = envelope(None, method, params);
        self.write_line(&payload).await
    }

    async fn write_line(&self, payload: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_string(payload)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.reader.abort();
        // `kill_on_drop(true)` already arms this; being explicit
        // makes teardown deterministic for tests.
        let _ = self.child.start_kill();
    }
}

/// Build a JSON-RPC 2.0 envelope. `params` is omitted when null so we
/// don't send `"params": null` to strict servers.
fn envelope(id: Option<u64>, method: &str, params: Value) -> Value {
    let mut obj = Map::new();
    obj.insert("jsonrpc".to_string(), Value::from("2.0"));
    if let Some(id) = id {
        obj.insert("id".to_string(), Value::from(id));
    }
    obj.insert("method".to_string(), Value::from(method));
    if !params.is_null() {
        obj.insert("params".to_string(), params);
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_env() -> HashMap<String, String> {
        HashMap::new()
    }

    /// Spawn a `bash -c` server that runs `script`.
    fn spawn_script(script: &str) -> StdioTransport {
        StdioTransport::spawn("bash", &["-c".to_string(), script.to_string()], &no_env())
            .expect("spawn bash server")
    }

    #[tokio::test]
    async fn transport_spawns_child_and_roundtrips_one_request() {
        // Reads one request line, answers with id 1 (the first id the
        // transport assigns).
        let t = spawn_script(
            "read _line; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}'",
        );
        let resp = t
            .request("ping", json!({}), Duration::from_secs(5))
            .await
            .expect("response");
        assert_eq!(resp["id"], json!(1));
        assert_eq!(resp["result"]["ok"], json!(true));
    }

    #[tokio::test]
    async fn transport_correlates_responses_to_request_ids_out_of_order() {
        // Server answers id 2 first, then id 1; each caller must get
        // its own response regardless of arrival order.
        let t = spawn_script(
            "read _a; read _b; \
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"n\":2}}'; \
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":1}}'",
        );
        let (r1, r2) = tokio::join!(
            t.request("first", json!({}), Duration::from_secs(5)),
            t.request("second", json!({}), Duration::from_secs(5)),
        );
        let r1 = r1.expect("first response");
        let r2 = r2.expect("second response");
        // first() got id 1, second() got id 2 — correlation, not order.
        assert_eq!(r1["id"], json!(1));
        assert_eq!(r1["result"]["n"], json!(1));
        assert_eq!(r2["id"], json!(2));
        assert_eq!(r2["result"]["n"], json!(2));
    }

    #[tokio::test]
    async fn transport_surfaces_spawn_failure_as_typed_error() {
        // `StdioTransport` (the Ok type) isn't `Debug`, so match the
        // Result directly rather than `expect_err`.
        let result = StdioTransport::spawn("anie-mcp-no-such-binary-zzzz", &[], &no_env());
        assert!(
            matches!(result, Err(McpError::Spawn { .. })),
            "expected a spawn error for a missing binary"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn transport_kills_child_on_drop() {
        let t = spawn_script("sleep 30");
        let pid = t.child_id().expect("running child has a pid");
        drop(t);

        let proc_path = format!("/proc/{pid}");
        let mut gone = false;
        for _ in 0..40 {
            if !std::path::Path::new(&proc_path).exists() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "child {pid} still present after transport drop");
    }

    #[tokio::test]
    async fn transport_times_out_when_child_never_responds() {
        // `sleep` never reads/writes our stdio, so the request is
        // never answered. (`cat` would be wrong — it echoes stdin
        // back, which the reader would mistake for a response.)
        let t =
            StdioTransport::spawn("sleep", &["30".to_string()], &no_env()).expect("spawn sleep");
        let result = t
            .request("never", json!({}), Duration::from_millis(200))
            .await;
        assert!(
            matches!(result, Err(McpError::Timeout { .. })),
            "request must time out"
        );
    }

    #[tokio::test]
    async fn request_after_server_exit_fails_fast_not_timeout() {
        // The server exits immediately; the reader sees EOF and marks the
        // transport closed. A subsequent request must fail fast with
        // Closed rather than blocking until its (here, very long) timeout.
        let t = spawn_script("exit 0");
        // Let the reader task observe EOF and set the closed flag.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let start = std::time::Instant::now();
        let result = t
            .request("ping", json!({}), Duration::from_secs(3600))
            .await;
        assert!(matches!(result, Err(McpError::Closed)), "{result:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not wait out the long timeout"
        );
    }
}
