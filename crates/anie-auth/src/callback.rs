//! Localhost HTTP callback server for OAuth `authorization_code`
//! flows.
//!
//! Anthropic redirects to `http://localhost:53692/callback?code=...&state=...`
//! after the user authorizes. We need *something* listening at
//! that URL to catch the code — otherwise the user sees a
//! browser "this site can't be reached" page and has to hand-
//! paste the URL.
//!
//! Implementation is deliberately minimal: one-shot, single
//! connection, handwritten HTTP parsing. No hyper/axum dep for
//! this tiny surface (the whole module is ~150 lines and runs
//! once per `anie login` invocation).
//!
//! pi's anthropic.ts uses Node's `http.createServer` for the
//! same purpose (`packages/ai/src/utils/oauth/anthropic.ts:98`).
//! We match its shape conceptually: listen, accept one
//! request, parse code + state, respond with a success page,
//! shut down.

use std::time::Duration;

use anyhow::{Result, anyhow};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, warn};

/// A code + state pair captured from the OAuth redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    /// Authorization code from the provider. Exchange via
    /// `OAuthProvider::complete_login`.
    pub code: String,
    /// State parameter; caller verifies it against the
    /// `LoginFlow.state` before using `code`.
    pub state: String,
}

/// Errors the callback server can surface.
#[derive(Debug, Error)]
pub enum CallbackError {
    /// The timeout elapsed before the user completed the
    /// browser step. Usually means they closed the tab or
    /// never got to it.
    #[error("OAuth callback timed out after {0:?}")]
    Timeout(Duration),
    /// Couldn't bind the requested port. Another instance of
    /// `anie login` is probably running.
    #[error("failed to bind callback port: {0}")]
    Bind(std::io::Error),
    /// Accept / read / write failures.
    #[error("callback I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Provider returned an `error=...` on the redirect instead
    /// of a code. Usually user-visible (e.g. "access_denied").
    #[error("OAuth provider returned error: {0}")]
    ProviderError(String),
    /// Request didn't include `code` or `state`, indicating the
    /// provider hit an endpoint that isn't the callback path.
    #[error("OAuth callback missing {missing}")]
    MissingParam { missing: String },
}

/// HTML rendered in the browser after a successful redirect.
/// Plain + self-closing so it works without JS or CSS.
const SUCCESS_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>anie — login complete</title></head><body style=\"font-family:system-ui,sans-serif;padding:2rem;\"><h1>anie</h1><p>Login complete. You can close this window and return to your terminal.</p></body></html>";

/// HTML rendered when the provider reports an error on
/// redirect. Keeps the page minimal and instructs the user
/// what to do.
fn error_html(detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>anie — login failed</title></head><body style=\"font-family:system-ui,sans-serif;padding:2rem;\"><h1>anie</h1><p>Login failed: {}</p><p>Return to your terminal and try again.</p></body></html>",
        html_escape(detail)
    )
}

/// Minimal HTML-escape to neutralize provider error strings
/// before echoing them into the response body. We only need to
/// handle `<`, `>`, `&` since we never inject inside an
/// attribute or script context.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Run a one-shot callback server on `127.0.0.1:port`. Blocks
/// until either a callback arrives or `timeout` elapses. Only
/// accepts the first connection to reach `/callback`; other
/// paths respond with 404 and the server keeps waiting.
pub async fn await_callback(port: u16, timeout: Duration) -> Result<Callback, CallbackError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(CallbackError::Bind)?;
    debug!(%port, "callback server listening");

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(CallbackError::Timeout(timeout));
        }
        let accept = tokio::time::timeout(remaining, listener.accept()).await;
        let (mut stream, _peer) = match accept {
            Ok(Ok(pair)) => pair,
            Ok(Err(err)) => return Err(CallbackError::Io(err)),
            Err(_) => return Err(CallbackError::Timeout(timeout)),
        };

        // Read a reasonable chunk — HTTP request line + headers
        // for a simple GET easily fits in 8 KiB.
        let mut buffer = vec![0u8; 8 * 1024];
        let n = stream.read(&mut buffer).await?;
        buffer.truncate(n);
        let request = String::from_utf8_lossy(&buffer);

        let Some(target) = parse_request_target(&request) else {
            // Malformed request — respond 400 and keep waiting
            // for the real callback.
            write_http_response(
                &mut stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                "bad request",
            )
            .await
            .ok();
            continue;
        };

        // Only `/callback` is our business. Other paths (e.g.
        // `/favicon.ico` — browsers fire these) get 404 and we
        // keep listening.
        let Some((path, query)) = split_target(&target) else {
            write_http_response(&mut stream, 404, "Not Found", "text/plain; charset=utf-8", "not found")
                .await
                .ok();
            continue;
        };
        if path != "/callback" {
            write_http_response(&mut stream, 404, "Not Found", "text/plain; charset=utf-8", "not found")
                .await
                .ok();
            continue;
        }

        let params: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap_or_default();

        if let Some((_, err_value)) = params.iter().find(|(k, _)| k == "error") {
            let html = error_html(err_value);
            write_http_response(&mut stream, 400, "Bad Request", "text/html; charset=utf-8", &html)
                .await
                .ok();
            return Err(CallbackError::ProviderError(err_value.clone()));
        }

        let code = params.iter().find(|(k, _)| k == "code").map(|(_, v)| v.clone());
        let state = params.iter().find(|(k, _)| k == "state").map(|(_, v)| v.clone());

        match (code, state) {
            (Some(code), Some(state)) => {
                write_http_response(&mut stream, 200, "OK", "text/html; charset=utf-8", SUCCESS_HTML)
                    .await
                    .ok();
                return Ok(Callback { code, state });
            }
            (None, _) => {
                write_http_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    "text/html; charset=utf-8",
                    &error_html("missing code parameter"),
                )
                .await
                .ok();
                return Err(CallbackError::MissingParam {
                    missing: "code".into(),
                });
            }
            (_, None) => {
                write_http_response(
                    &mut stream,
                    400,
                    "Bad Request",
                    "text/html; charset=utf-8",
                    &error_html("missing state parameter"),
                )
                .await
                .ok();
                return Err(CallbackError::MissingParam {
                    missing: "state".into(),
                });
            }
        }
    }
}

/// Parse `GET /callback?foo=bar HTTP/1.1` → `/callback?foo=bar`.
fn parse_request_target(request: &str) -> Option<String> {
    let first_line = request.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    Some(target.to_string())
}

/// Split `/callback?foo=bar` → (`/callback`, `foo=bar`). Both
/// halves are borrowed; target with no `?` returns empty query.
fn split_target(target: &str) -> Option<(&str, &str)> {
    match target.split_once('?') {
        Some((path, query)) => Some((path, query)),
        None => Some((target, "")),
    }
}

async fn write_http_response(
    stream: &mut tokio::net::TcpStream,
    status_code: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status_code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|err| anyhow!("failed to write response: {err}"))?;
    stream.shutdown().await.ok();
    if let Err(err) = stream.flush().await {
        warn!(%err, "failed to flush callback response");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream as BlockingTcpStream;
    use std::time::Duration;

    #[tokio::test]
    async fn await_callback_captures_code_and_state_from_query_string() {
        // Let the OS pick a port so parallel tests don't collide.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let server = tokio::spawn(await_callback(port, Duration::from_secs(5)));

        // Wait for the callback server to actually be listening.
        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect");
            let request = "GET /callback?code=the-code&state=the-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
            stream.write_all(request.as_bytes()).expect("write");
            // Read the response so the server writes back without panicking.
            use std::io::Read;
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
        })
        .await
        .expect("send request");

        let callback = server.await.expect("join").expect("callback");
        assert_eq!(callback.code, "the-code");
        assert_eq!(callback.state, "the-state");
    }

    #[tokio::test]
    async fn await_callback_surfaces_provider_error_parameter() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let server = tokio::spawn(await_callback(port, Duration::from_secs(5)));

        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stream = BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect");
            let request = "GET /callback?error=access_denied HTTP/1.1\r\nHost: localhost\r\n\r\n";
            stream.write_all(request.as_bytes()).expect("write");
            use std::io::Read;
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
        })
        .await
        .expect("send request");

        let err = server.await.expect("join").unwrap_err();
        assert!(
            matches!(err, CallbackError::ProviderError(ref m) if m == "access_denied"),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn await_callback_times_out_when_no_request_arrives() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let err = await_callback(port, Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(matches!(err, CallbackError::Timeout(_)), "{err:?}");
    }

    #[tokio::test]
    async fn await_callback_ignores_non_callback_paths_and_keeps_waiting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let server = tokio::spawn(await_callback(port, Duration::from_secs(5)));

        tokio::time::sleep(Duration::from_millis(100)).await;

        // First a stray /favicon.ico request — should 404 and not
        // terminate the server.
        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut stream = BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect 1");
            stream
                .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("write 1");
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            drop(stream);

            // Then the real /callback. Server should pick this up.
            let mut stream = BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect 2");
            stream
                .write_all(
                    b"GET /callback?code=c&state=s HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .expect("write 2");
            let _ = stream.read(&mut buf);
        })
        .await
        .expect("send requests");

        let callback = server.await.expect("join").expect("callback");
        assert_eq!(callback.code, "c");
        assert_eq!(callback.state, "s");
    }
}
