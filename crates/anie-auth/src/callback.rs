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
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
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

/// Localhost callback clients should send a tiny GET immediately after
/// connecting. Keep this shorter than the overall OAuth timeout so an
/// idle local connection cannot consume the entire login window.
const ACCEPTED_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Response writes are tiny, so a stalled local peer should not
/// consume the overall OAuth callback window.
const ACCEPTED_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

struct HttpResponse<'a> {
    status_code: u16,
    reason: &'a str,
    content_type: &'a str,
    body: &'a str,
    context: &'a str,
}

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

/// Legacy convenience wrapper — matches the pre-multi-provider
/// shape where Anthropic's `/callback` path was assumed. New
/// callers should use `await_callback_on_path`.
pub async fn await_callback(port: u16, timeout: Duration) -> Result<Callback, CallbackError> {
    await_callback_on_path(port, "/callback", timeout).await
}

/// Run a one-shot callback server on `127.0.0.1:port`. Blocks
/// until either a callback arrives or `timeout` elapses. Only
/// accepts the first connection to reach `expected_path`; other
/// paths respond with 404 and the server keeps waiting.
///
/// Takes the expected path as a parameter because different
/// providers register different callback routes (Anthropic:
/// `/callback`, OpenAI Codex: `/auth/callback`, Google:
/// `/oauth/callback`).
pub async fn await_callback_on_path(
    port: u16,
    expected_path: &str,
    timeout: Duration,
) -> Result<Callback, CallbackError> {
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

        let Some(buffer) = read_request_chunk(&mut stream, deadline, timeout).await? else {
            continue;
        };
        let request = String::from_utf8_lossy(&buffer);

        let Some(target) = parse_request_target(&request) else {
            // Malformed request — respond 400 and keep waiting
            // for the real callback.
            write_http_response_best_effort(
                &mut stream,
                deadline,
                timeout,
                HttpResponse {
                    status_code: 400,
                    reason: "Bad Request",
                    content_type: "text/plain; charset=utf-8",
                    body: "bad request",
                    context: "malformed callback request",
                },
            )
            .await;
            continue;
        };

        // Only `/callback` is our business. Other paths (e.g.
        // `/favicon.ico` — browsers fire these) get 404 and we
        // keep listening.
        let Some((path, query)) = split_target(&target) else {
            write_http_response_best_effort(
                &mut stream,
                deadline,
                timeout,
                HttpResponse {
                    status_code: 404,
                    reason: "Not Found",
                    content_type: "text/plain; charset=utf-8",
                    body: "not found",
                    context: "callback request without path",
                },
            )
            .await;
            continue;
        };
        if path != expected_path {
            write_http_response_best_effort(
                &mut stream,
                deadline,
                timeout,
                HttpResponse {
                    status_code: 404,
                    reason: "Not Found",
                    content_type: "text/plain; charset=utf-8",
                    body: "not found",
                    context: "callback request for unexpected path",
                },
            )
            .await;
            continue;
        }

        let params: Vec<(String, String)> = serde_urlencoded::from_str(query).unwrap_or_default();

        if let Some((_, err_value)) = params.iter().find(|(k, _)| k == "error") {
            let html = error_html(err_value);
            write_http_response_best_effort(
                &mut stream,
                deadline,
                timeout,
                HttpResponse {
                    status_code: 400,
                    reason: "Bad Request",
                    content_type: "text/html; charset=utf-8",
                    body: &html,
                    context: "provider error callback",
                },
            )
            .await;
            return Err(CallbackError::ProviderError(err_value.clone()));
        }

        let code = params
            .iter()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.clone());
        let state = params
            .iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.clone());

        match (code, state) {
            (Some(code), Some(state)) => {
                write_http_response_best_effort(
                    &mut stream,
                    deadline,
                    timeout,
                    HttpResponse {
                        status_code: 200,
                        reason: "OK",
                        content_type: "text/html; charset=utf-8",
                        body: SUCCESS_HTML,
                        context: "successful callback",
                    },
                )
                .await;
                return Ok(Callback { code, state });
            }
            (None, _) => {
                let html = error_html("missing code parameter");
                write_http_response_best_effort(
                    &mut stream,
                    deadline,
                    timeout,
                    HttpResponse {
                        status_code: 400,
                        reason: "Bad Request",
                        content_type: "text/html; charset=utf-8",
                        body: &html,
                        context: "callback missing code",
                    },
                )
                .await;
                return Err(CallbackError::MissingParam {
                    missing: "code".into(),
                });
            }
            (_, None) => {
                let html = error_html("missing state parameter");
                write_http_response_best_effort(
                    &mut stream,
                    deadline,
                    timeout,
                    HttpResponse {
                        status_code: 400,
                        reason: "Bad Request",
                        content_type: "text/html; charset=utf-8",
                        body: &html,
                        context: "callback missing state",
                    },
                )
                .await;
                return Err(CallbackError::MissingParam {
                    missing: "state".into(),
                });
            }
        }
    }
}

async fn read_request_chunk(
    stream: &mut tokio::net::TcpStream,
    deadline: tokio::time::Instant,
    overall_timeout: Duration,
) -> Result<Option<Vec<u8>>, CallbackError> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(CallbackError::Timeout(overall_timeout));
    }

    // Read a reasonable chunk — HTTP request line + headers for a
    // simple GET easily fits in 8 KiB.
    let mut buffer = vec![0u8; 8 * 1024];
    match tokio::time::timeout(
        remaining.min(ACCEPTED_READ_TIMEOUT),
        stream.read(&mut buffer),
    )
    .await
    {
        Ok(Ok(n)) => {
            buffer.truncate(n);
            Ok(Some(buffer))
        }
        Ok(Err(err)) => Err(CallbackError::Io(err)),
        Err(_) => Ok(None),
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

async fn write_http_response_best_effort<W>(
    stream: &mut W,
    deadline: tokio::time::Instant,
    overall_timeout: Duration,
    response: HttpResponse<'_>,
) where
    W: AsyncWrite + Unpin,
{
    let context = response.context;
    if let Err(err) = write_http_response(stream, deadline, overall_timeout, response).await {
        warn!(%err, context, "failed to write callback response");
    }
}

async fn write_http_response<W>(
    stream: &mut W,
    deadline: tokio::time::Instant,
    overall_timeout: Duration,
    response: HttpResponse<'_>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(anyhow!(
            "callback response deadline elapsed after {overall_timeout:?}"
        ));
    }

    let response = format!(
        "HTTP/1.1 {status_code} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        status_code = response.status_code,
        reason = response.reason,
        content_type = response.content_type,
        len = response.body.len(),
        body = response.body,
    );

    match tokio::time::timeout(
        remaining.min(ACCEPTED_WRITE_TIMEOUT),
        stream.write_all(response.as_bytes()),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(anyhow!("failed to write response: {err}")),
        Err(_) => return Err(anyhow!("timed out writing callback response")),
    }

    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if !remaining.is_zero() {
        match tokio::time::timeout(remaining.min(ACCEPTED_WRITE_TIMEOUT), stream.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(%err, "failed to shutdown callback response"),
            Err(_) => return Err(anyhow!("timed out shutting down callback response")),
        }
    }

    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if !remaining.is_zero() {
        match tokio::time::timeout(remaining.min(ACCEPTED_WRITE_TIMEOUT), stream.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(%err, "failed to flush callback response"),
            Err(_) => return Err(anyhow!("timed out flushing callback response")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream as BlockingTcpStream;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;

    struct StalledWriter;

    impl AsyncWrite for StalledWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

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
            let request =
                "GET /callback?code=the-code&state=the-state HTTP/1.1\r\nHost: localhost\r\n\r\n";
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
    async fn await_callback_ignores_idle_connection_and_accepts_later_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let server = tokio::spawn(await_callback(port, Duration::from_secs(5)));

        tokio::time::sleep(Duration::from_millis(100)).await;

        let idle = tokio::task::spawn_blocking(move || {
            BlockingTcpStream::connect(("127.0.0.1", port)).expect("idle connect")
        })
        .await
        .expect("idle task");

        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut stream = BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            stream
                .write_all(b"GET /callback?code=late-code&state=late-state HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("write valid callback");
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
        })
        .await
        .expect("send valid callback");
        drop(idle);

        let callback = server.await.expect("join").expect("callback");
        assert_eq!(callback.code, "late-code");
        assert_eq!(callback.state, "late-state");
    }

    #[tokio::test]
    async fn malformed_request_gets_best_effort_400_before_later_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind probe");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let server = tokio::spawn(await_callback(port, Duration::from_secs(5)));

        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut malformed =
                BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect malformed");
            malformed
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            malformed
                .write_all(b"not-http\r\n\r\n")
                .expect("write malformed request");
            let mut buf = [0u8; 512];
            let n = malformed.read(&mut buf).expect("read malformed response");
            let response = String::from_utf8_lossy(&buf[..n]);
            assert!(
                response.starts_with("HTTP/1.1 400 Bad Request"),
                "{response}"
            );
            drop(malformed);

            let mut valid =
                BlockingTcpStream::connect(("127.0.0.1", port)).expect("connect valid");
            valid
                .write_all(
                    b"GET /callback?code=after-malformed&state=ok HTTP/1.1\r\nHost: localhost\r\n\r\n",
                )
                .expect("write valid callback");
            let _ = valid.read(&mut buf);
        })
        .await
        .expect("send requests");

        let callback = server.await.expect("join").expect("callback");
        assert_eq!(callback.code, "after-malformed");
        assert_eq!(callback.state, "ok");
    }

    #[tokio::test]
    async fn write_http_response_times_out_on_stalled_writer() {
        let mut writer = StalledWriter;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(50);

        let err = write_http_response(
            &mut writer,
            deadline,
            Duration::from_millis(50),
            HttpResponse {
                status_code: 400,
                reason: "Bad Request",
                content_type: "text/plain; charset=utf-8",
                body: "bad request",
                context: "test response",
            },
        )
        .await
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("timed out writing callback response"),
            "{err:?}"
        );
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
                .write_all(b"GET /callback?code=c&state=s HTTP/1.1\r\nHost: localhost\r\n\r\n")
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
