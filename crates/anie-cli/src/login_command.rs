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
    AnthropicOAuthProvider, AuthCredential, CallbackError, CredentialStore, OAuthCredentialData,
    OAuthProvider, await_callback,
};

/// Port the callback server binds. Anthropic's OAuth client
/// registration fixes this to 53692 — pi uses the same value,
/// see `crates/anie-auth/src/anthropic_oauth.rs`.
const CALLBACK_PORT: u16 = 53692;

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

    println!(
        "Open the following URL in your browser to authorize anie:\n\n  {url}\n",
        url = flow.authorize_url,
    );
    println!(
        "Waiting for the redirect at {redirect} (timeout: {secs}s)...",
        redirect = flow.redirect_uri,
        secs = LOGIN_TIMEOUT.as_secs(),
    );

    let callback = await_callback(CALLBACK_PORT, LOGIN_TIMEOUT)
        .await
        .map_err(translate_callback_error)?;

    // Verify state before exchanging — rejects CSRF-style
    // attacks where someone feeds us a foreign redirect URL.
    if callback.state != flow.state {
        return Err(anyhow!(
            "OAuth state mismatch: expected {expected}, got {got}. \
             This can mean a stale browser tab completed the flow — \
             re-run `anie login {provider_name}` and try again.",
            expected = redact_state(&flow.state),
            got = redact_state(&callback.state),
        ));
    }

    let credential_data = provider
        .complete_login(&flow, &callback.code)
        .await
        .context("token exchange failed")?;

    persist_credential(&store, provider_name, &credential_data)?;

    if let Some(account) = credential_data.account.as_deref() {
        println!("Logged in to {provider_name} as {account}.");
    } else {
        println!("Logged in to {provider_name}. Access token stored.");
    }
    println!("Token expires at {} (local refresh is automatic).", credential_data.expires_at);
    Ok(())
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
        other => Err(anyhow!(
            "'{other}' does not support OAuth login. \
             Supported providers: anthropic."
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
            "failed to bind callback port {CALLBACK_PORT}: {err}. \
             Another instance of `anie login` may already be running."
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
    fn build_oauth_provider_supports_anthropic_only_for_now() {
        assert!(build_oauth_provider("anthropic").is_ok());
        // `Box<dyn OAuthProvider>` doesn't impl Debug, so we
        // can't use unwrap_err directly; grab the error via
        // pattern match instead.
        let err = match build_oauth_provider("openai") {
            Ok(_) => panic!("openai should not be supported yet"),
            Err(err) => err.to_string(),
        };
        assert!(err.contains("OAuth"), "{err}");
        assert!(err.contains("anthropic"), "{err}");
    }
}
