//! Headless Chrome rendering for `web_read` when
//! `javascript: true`.
//!
//! Implementation notes:
//!
//! - We don't bundle Chrome. The agent's host system is
//!   expected to have `chromium`, `google-chrome`, `chrome`, or
//!   a path supplied via `CHROME_PATH`. This mirrors the
//!   Defuddle subprocess approach: explicit, documented runtime
//!   dependency.
//! - Chrome discovery is always compiled (no feature gate) so
//!   we can produce a clear "not installed" error even on a
//!   default build, where `javascript: true` is rejected before
//!   we ever try to launch the browser.
//! - Render is gated behind the `headless` cargo feature so
//!   default builds don't pull in `chromiumoxide`'s dep tree.
//!
//! The render path returns post-DOM HTML that the existing
//! Defuddle bridge can consume unchanged — Defuddle doesn't
//! know or care that the HTML came from a JS-rendered page.

use std::path::PathBuf;

use crate::error::WebToolError;

/// Locate a Chrome / Chromium binary on the host. Returns the
/// path on success, or [`WebToolError::HeadlessFailure`] with
/// an install hint when nothing is found.
///
/// Lookup order (matches the design doc):
///
/// 1. `CHROME_PATH` env var (operator override).
/// 2. `which::which` for `chromium`, `chromium-browser`,
///    `google-chrome`, `chrome` — typical Linux package names
///    plus the `chrome` symlink some distros ship.
/// 3. macOS standard install path.
///
/// We don't search Windows-specific paths; users on Windows
/// can set `CHROME_PATH` explicitly.
pub fn locate_chrome() -> Result<PathBuf, WebToolError> {
    if let Ok(path) = std::env::var("CHROME_PATH") {
        let candidate = PathBuf::from(&path);
        if candidate.exists() {
            return Ok(candidate);
        }
        return Err(WebToolError::HeadlessFailure(format!(
            "CHROME_PATH points to a missing file: {path}"
        )));
    }

    for candidate in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
        if let Ok(path) = which::which(candidate) {
            return Ok(path);
        }
    }

    let mac_default = PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    if mac_default.exists() {
        return Ok(mac_default);
    }

    Err(WebToolError::HeadlessFailure(
        "Chrome / Chromium not found. Install one of `chromium`, `google-chrome`, or set CHROME_PATH to a binary."
            .into(),
    ))
}

/// Render `url` in a fresh headless browser tab and return the
/// post-DOM HTML. The caller hands the result to Defuddle just
/// like a regular fetch.
///
/// `timeout` bounds the entire render — launch + navigate +
/// capture. Hanging pages produce a [`WebToolError::Timeout`]
/// rather than blocking the agent indefinitely.
///
/// Available only with `--features headless`. The matching
/// stub on the no-feature path lives in `tool.rs`, where
/// `javascript: true` is rejected up front with a build hint.
///
/// **SSRF posture (PR 3.3 of
/// `docs/code_review_2026-04-27/`).** This function is **not**
/// SSRF-equivalent to [`crate::read::fetch::fetch_html`]. The
/// caller is expected to have run
/// [`crate::read::fetch::validate_destination`] on `url` before
/// invoking this function — that covers the initial navigation.
/// Once Chrome is running, however, it follows redirects and
/// loads subresources (CSS, images, XHR) through its own
/// network stack. anie does not currently install a CDP
/// request-interception handler, so a malicious page can
/// trigger requests to `127.0.0.1` or RFC 1918 hosts that the
/// non-headless path would have refused.
///
/// Operators enabling `--features headless` and exposing
/// `javascript=true` to the agent should treat this as an
/// explicit escape hatch: the safety guarantees of the
/// non-headless `web_read` do not transfer.
#[cfg(feature = "headless")]
pub async fn render_with_chrome(
    url: &url::Url,
    timeout: std::time::Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, WebToolError> {
    use chromiumoxide::browser::{Browser, BrowserConfig};
    use futures::StreamExt;

    if cancel.is_cancelled() {
        return Err(WebToolError::Aborted);
    }

    // Per the doc comment above: the headless path is not SSRF
    // equivalent to the non-headless path. Log this loudly so
    // operators see it whenever `javascript=true` actually
    // launches Chrome, not just buried in docs.
    tracing::warn!(
        target = %url,
        "headless render: subresources/redirects loaded by Chrome are NOT guarded against private destinations; see render_with_chrome docs"
    );

    let chrome_path = locate_chrome()?;

    // chromiumoxide defaults to `/tmp/chromiumoxide-runner` for
    // the user-data-dir. A second launch in the same session
    // finds the previous run's `SingletonLock` and aborts with
    // "Failed to create a ProcessSingleton for your profile
    // directory". Give every launch its own tempdir; drop it
    // after Chrome shuts down. (Caught by smoke runs against
    // qwen3.6 — once the agent makes two `javascript: true`
    // calls in a row, the second one was failing 100% of the
    // time on shared CI-style hosts.)
    let user_data_dir = tempfile::Builder::new()
        .prefix("anie-chrome-")
        .tempdir()
        .map_err(|e| WebToolError::HeadlessFailure(format!("user-data-dir: {e}")))?;

    let render = async {
        let config = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .user_data_dir(user_data_dir.path())
            .build()
            .map_err(|e| WebToolError::HeadlessFailure(format!("browser config: {e}")))?;

        let (mut browser, mut handler) = Browser::launch(config)
            .await
            .map_err(|e| WebToolError::HeadlessFailure(format!("launch: {e}")))?;

        // chromiumoxide requires the event handler stream to be
        // polled; otherwise CDP messages never resolve. Spawn
        // it for the lifetime of the render and abort when we
        // close the browser.
        let handler_task = tokio::spawn(async move {
            while let Some(_event) = handler.next().await {
                // Drain events; we don't act on them here.
            }
        });

        let result: Result<String, WebToolError> = async {
            let page = browser
                .new_page(url.as_str())
                .await
                .map_err(|e| WebToolError::HeadlessFailure(format!("new page: {e}")))?;
            page.wait_for_navigation()
                .await
                .map_err(|e| WebToolError::HeadlessFailure(format!("navigation: {e}")))?;
            // wait_for_navigation only resolves on the document
            // `load` event. SPAs typically render the article
            // body via XHR fired after that, so capturing
            // immediately gets the empty shell. A short fixed
            // grace period covers the common case without
            // building a full network-idle tracker. Bounded by
            // the outer `tokio::time::timeout`.
            tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
            let html = page
                .content()
                .await
                .map_err(|e| WebToolError::HeadlessFailure(format!("content: {e}")))?;
            Ok(html)
        }
        .await;

        // Always close, even on render error, so we don't leak
        // a browser process.
        let _ = browser.close().await;
        handler_task.abort();
        result
    };

    // Race timeout, cancellation, and the render. The render
    // closure handles browser cleanup on its own success and
    // internal-error paths via `browser.close().await`. If
    // either timeout *or* cancellation wins here, the render
    // future is dropped before `browser.close()` can run, and
    // chromiumoxide does NOT tear down Chrome on `Browser`
    // drop — so the spawned process is leaked until anie's
    // own exit reaps it. The same leak existed for the
    // timeout path before this PR; cancellation just adds a
    // second trigger. Closing this requires a top-level
    // `Browser` handle so we can call `.kill()` from outside
    // the inner future, plus rolling our own select against
    // an explicit timer rather than `tokio::time::timeout`.
    // Tracked as follow-up to PR 4.1.
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(WebToolError::Aborted),
        result = tokio::time::timeout(timeout, render) => match result {
            Ok(res) => res,
            Err(_) => Err(WebToolError::Timeout { seconds: timeout.as_secs() }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_chrome_returns_install_message_when_nothing_found() {
        // Sandbox: temporarily clear CHROME_PATH and PATH so
        // neither `which` nor the env-var path can succeed.
        // Because Rust tests share a process, we restore the
        // originals on drop.
        struct EnvGuard {
            saved_chrome: Option<String>,
            saved_path: Option<String>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                if let Some(v) = &self.saved_chrome {
                    // SAFETY: tests run with a single-threaded
                    // env. The guard restores prior state.
                    unsafe { std::env::set_var("CHROME_PATH", v) };
                } else {
                    unsafe { std::env::remove_var("CHROME_PATH") };
                }
                if let Some(v) = &self.saved_path {
                    unsafe { std::env::set_var("PATH", v) };
                } else {
                    unsafe { std::env::remove_var("PATH") };
                }
            }
        }
        let _guard = EnvGuard {
            saved_chrome: std::env::var("CHROME_PATH").ok(),
            saved_path: std::env::var("PATH").ok(),
        };
        // SAFETY: see note above.
        unsafe {
            std::env::remove_var("CHROME_PATH");
            std::env::set_var("PATH", "");
        }

        // The test still passes if the host has Chrome at the
        // macOS default path; that's a real install and a
        // legitimate "found" result. Skip the assertion in that
        // case rather than fail.
        let mac_default =
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        if mac_default.exists() {
            return;
        }

        let err = locate_chrome().unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, WebToolError::HeadlessFailure(_)),
            "expected HeadlessFailure, got: {err:?}"
        );
        assert!(
            msg.contains("CHROME_PATH") || msg.contains("not found"),
            "expected install hint, got: {msg}"
        );
    }

    #[test]
    fn locate_chrome_rejects_chrome_path_pointing_to_missing_file() {
        struct EnvGuard {
            saved: Option<String>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                if let Some(v) = &self.saved {
                    unsafe { std::env::set_var("CHROME_PATH", v) };
                } else {
                    unsafe { std::env::remove_var("CHROME_PATH") };
                }
            }
        }
        let _guard = EnvGuard {
            saved: std::env::var("CHROME_PATH").ok(),
        };
        unsafe {
            std::env::set_var(
                "CHROME_PATH",
                "/definitely/does/not/exist/chrome-binary-xyz",
            );
        }

        let err = locate_chrome().unwrap_err();
        match err {
            WebToolError::HeadlessFailure(msg) => {
                assert!(msg.contains("CHROME_PATH"), "msg: {msg}");
            }
            other => panic!("expected HeadlessFailure, got: {other:?}"),
        }
    }
}
