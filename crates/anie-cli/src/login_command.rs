//! `anie login <provider>` / `anie logout <provider>` CLI
//! subcommands.
//!
//! `login` runs the OAuth Authorization Code + PKCE flow:
//! opens the authorize URL, starts a localhost callback
//! server, awaits the redirect, exchanges the code for a
//! credential pair, persists to `auth.json`.
//!
//! `logout` removes the provider's credential from the store.
//!
//! Only `anthropic` is wired up for now — `anie-auth` resolves
//! the concrete OAuth client. Add a match arm when a second
//! provider lands.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use anie_auth::{
    AnthropicOAuthProvider, AuthCredential, CallbackError, CredentialStore, LoginFlow,
    OAuthCredentialData, OAuthProvider, OpenAICodexOAuthProvider, await_callback_on_path,
};

/// Fallback callback port when the provider's redirect URI
/// doesn't include one (shouldn't happen for the flows we
/// support today, but keeps the path safe). Matches Anthropic's
/// registered port.
const DEFAULT_CALLBACK_PORT: u16 = 53692;

/// Upper bound on how long we'll wait for the user to complete
/// the browser step. 5 minutes is generous enough for real
/// users (including 2FA) while still timing out hung flows.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Run the login flow for `provider_name`. Prints progress to
/// stdout; the user opens the URL in their browser manually
/// (no `opener` crate yet — first-pass UX).
pub async fn run_login(provider_name: &str) -> Result<()> {
    let provider = build_oauth_provider(provider_name)?;
    let store = CredentialStore::new();

    let flow = provider
        .begin_login()
        .await
        .context("failed to start OAuth login flow")?;

    let credential_data = match &flow {
        LoginFlow::AuthorizationCode(auth_code) => {
            let opened = try_open_browser(&auth_code.authorize_url);
            print_login_prompt(&auth_code.authorize_url, opened, LOGIN_TIMEOUT);

            // Callback port + path are derived from the
            // redirect_uri the provider published, so each
            // provider can listen on its own registered route.
            let (port, path) = callback_route_from_uri(&auth_code.redirect_uri)
                .unwrap_or((DEFAULT_CALLBACK_PORT, "/callback".to_string()));
            let callback = await_callback_on_path(port, &path, LOGIN_TIMEOUT)
                .await
                .map_err(translate_callback_error)?;

            if callback.state != auth_code.state {
                return Err(anyhow!(
                    "OAuth state mismatch: expected {expected}, got {got}. \
                     This can mean a stale browser tab completed the flow — \
                     re-run `anie login {provider_name}` and try again.",
                    expected = redact_state(&auth_code.state),
                    got = redact_state(&callback.state),
                ));
            }

            provider
                .complete_login(&flow, Some(&callback.code))
                .await
                .context("token exchange failed")?
        }
        LoginFlow::Device(device) => {
            print_device_prompt(device);
            if let Some(complete_uri) = &device.verification_uri_complete {
                // Auto-open the "complete" URL if the provider
                // published one — saves the user from typing.
                try_open_browser(complete_uri);
            } else {
                try_open_browser(&device.verification_uri);
            }
            provider
                .complete_login(&flow, None)
                .await
                .context("device-flow polling failed")?
        }
    };

    persist_credential(&store, provider_name, &credential_data)?;

    if let Some(account) = credential_data.account.as_deref() {
        println!("Logged in to {provider_name} as {account}.");
    } else {
        println!("Logged in to {provider_name}. Access token stored.");
    }
    println!(
        "Token expires at {} (local refresh is automatic).",
        credential_data.expires_at
    );
    Ok(())
}

/// Pull the port and path out of `http://localhost:<port>/<path>`.
/// Used when each OAuth provider publishes a different callback
/// route — Anthropic /callback:53692, Codex /auth/callback:1455,
/// Antigravity /:51121, Gemini /oauth2callback:8085.
fn callback_route_from_uri(uri: &str) -> Option<(u16, String)> {
    let rest = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    let (host_port, path_suffix) = match rest.split_once('/') {
        Some((hp, p)) => (hp, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let (_, port_str) = host_port.rsplit_once(':')?;
    let port = port_str.parse().ok()?;
    Some((port, path_suffix))
}

fn print_device_prompt(device: &anie_auth::DeviceCodeFlow) {
    println!("Device-code authorization required:");
    println!("  Open:   {}", device.verification_uri);
    println!("  Enter:  {}", device.user_code);
    println!(
        "  Waiting for authorization (expires in {}s)",
        device.expires_in.as_secs()
    );
}

/// Run the logout flow for `provider_name`.
pub async fn run_logout(provider_name: &str) -> Result<()> {
    let store = CredentialStore::new();
    match store.get_credential(provider_name) {
        None => {
            println!("No stored credential for {provider_name}. Nothing to remove.");
        }
        Some(_) => {
            store
                .delete(provider_name)
                .with_context(|| format!("failed to remove credential for {provider_name}"))?;
            println!("Removed stored credential for {provider_name}.");
        }
    }
    Ok(())
}

/// Try to open the authorize URL in the user's default
/// browser. Returns `true` on success, `false` on headless /
/// sandboxed environments where no browser handler is
/// registered — in those cases the caller prints the URL so
/// the user can paste it into a browser on another machine.
fn try_open_browser(url: &str) -> bool {
    match opener::open_browser(url) {
        Ok(()) => true,
        Err(err) => {
            // Don't treat a missing browser as fatal; the
            // flow still works if the user manually opens the
            // URL, just less conveniently.
            tracing::debug!(%err, "could not auto-open browser");
            false
        }
    }
}

/// Print a compact login banner. When the browser auto-opened
/// we collapse the URL onto a single "Paste if needed:" line so
/// the terminal stays readable; headless cases present the URL
/// prominently on its own line.
fn print_login_prompt(url: &str, opened: bool, timeout: std::time::Duration) {
    let secs = timeout.as_secs();
    if opened {
        println!("Opening browser for OAuth authorization...");
        println!("  Paste if needed: {url}");
        println!("  Waiting for redirect (timeout {secs}s)");
    } else {
        println!("OAuth authorization required.");
        println!("  Open this URL in a browser:");
        println!("    {url}");
        println!("  Waiting for redirect (timeout {secs}s)");
    }
}

/// Redact a state/verifier string to a short prefix + suffix so
/// log output doesn't leak the full value. PKCE verifiers are
/// 43 chars; we show the first 4 and last 4.
fn redact_state(value: &str) -> String {
    if value.len() <= 12 {
        // Too short to usefully redact; return as-is so the
        // mismatch is debuggable.
        return value.to_string();
    }
    let prefix: String = value.chars().take(4).collect();
    let suffix: String = value.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{prefix}…{suffix}")
}

fn persist_credential(
    store: &CredentialStore,
    provider_name: &str,
    data: &OAuthCredentialData,
) -> Result<()> {
    let credential = AuthCredential::OAuth {
        access_token: data.access_token.clone(),
        refresh_token: data.refresh_token.clone(),
        expires_at: data.expires_at.clone(),
        account: data.account.clone(),
        api_base_url: data.api_base_url.clone(),
        project_id: data.project_id.clone(),
    };
    store
        .set_credential(provider_name, credential)
        .with_context(|| format!("failed to save {provider_name} credential to auth.json"))
}

/// Map provider name → concrete OAuth client. This module
/// owns its own mapping (instead of reusing a hypothetical
/// registry elsewhere) because the CLI login path needs a
/// fully-fledged client, not just "does this provider support
/// OAuth" — if a second provider joins, add its arm here.
fn build_oauth_provider(provider_name: &str) -> Result<Box<dyn OAuthProvider>> {
    match provider_name {
        "anthropic" => Ok(Box::new(AnthropicOAuthProvider::new())),
        "openai-codex" => Ok(Box::new(OpenAICodexOAuthProvider::new())),
        other => Err(anyhow!(
            "'{other}' does not support OAuth login. \
             Supported providers: anthropic, openai-codex."
        )),
    }
}

fn translate_callback_error(err: CallbackError) -> anyhow::Error {
    match err {
        CallbackError::Timeout(d) => anyhow!(
            "OAuth login timed out after {secs}s. \
             Re-run `anie login` and complete the flow in the browser.",
            secs = d.as_secs(),
        ),
        CallbackError::Bind(err) => anyhow!(
            "failed to bind callback port: {err}. \
             Another instance of `anie login` may already be running, \
             or the port is in use by something else."
        ),
        CallbackError::ProviderError(message) => anyhow!(
            "provider rejected the login request: {message}"
        ),
        other => anyhow!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_state_leaves_short_values_readable() {
        assert_eq!(redact_state("abc"), "abc");
        assert_eq!(redact_state("12345"), "12345");
    }

    #[test]
    fn redact_state_shows_prefix_and_suffix_for_long_values() {
        let verifier = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG";
        assert_eq!(redact_state(verifier), "abcd…DEFG");
    }

    #[test]
    fn build_oauth_provider_accepts_registered_providers() {
        assert!(build_oauth_provider("anthropic").is_ok());
        assert!(build_oauth_provider("openai-codex").is_ok());
    }

    #[test]
    fn build_oauth_provider_rejects_unknown_names() {
        // `Box<dyn OAuthProvider>` doesn't impl Debug, so we
        // can't use unwrap_err directly; grab the error via
        // pattern match instead.
        let err = match build_oauth_provider("made-up") {
            Ok(_) => panic!("made-up should not be supported"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("OAuth"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
        assert!(err.contains("openai-codex"), "{err}");
    }

    #[test]
    fn callback_route_from_uri_parses_port_and_path() {
        assert_eq!(
            callback_route_from_uri("http://localhost:53692/callback"),
            Some((53692, "/callback".to_string()))
        );
        assert_eq!(
            callback_route_from_uri("http://localhost:1455/auth/callback"),
            Some((1455, "/auth/callback".to_string()))
        );
    }

    #[test]
    fn callback_route_from_uri_rejects_non_http_urls() {
        assert!(callback_route_from_uri("not a url").is_none());
        assert!(callback_route_from_uri("ftp://host:1234/").is_none());
    }
}
