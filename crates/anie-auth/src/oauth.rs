//! Generic OAuth provider trait + shared helpers.
//!
//! This module defines the `OAuthProvider` trait that every
//! provider-specific OAuth implementation (Anthropic, future
//! Google / GitHub) slots into. It also carries the pieces that
//! are protocol-level rather than provider-specific:
//!
//! - PKCE helpers (code_verifier generation, SHA-256 challenge).
//! - `LoginFlow` and `OAuthCredentialData` types shared across
//!   providers.
//! - RFC 3339 expiry formatting.
//!
//! Per-provider endpoints + client IDs live in their own
//! `*_oauth.rs` files (`anthropic_oauth.rs`, etc.). This keeps
//! drift isolated when one provider changes its flow.
//!
//! Shape-aligned with pi's `packages/ai/src/utils/oauth/` layout.
//! Verified against pi's pkce.ts + anthropic.ts as of 2026-04-21.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Handoff value returned by `OAuthProvider::begin_login`. The
/// caller shows `authorize_url` to the user (print it, open a
/// browser, etc.), waits for the code to arrive via the
/// redirect callback (or manual paste), then calls
/// `complete_login` with the same `LoginFlow` so PKCE state
/// stays matched.
#[derive(Debug, Clone)]
pub struct LoginFlow {
    /// URL the user opens to authorize anie. Pre-signed with
    /// client_id + redirect_uri + scope + PKCE challenge.
    pub authorize_url: String,
    /// PKCE code_verifier. Kept private to this machine — the
    /// token-exchange call proves possession without ever
    /// revealing it in the browser.
    pub verifier: String,
    /// State parameter for CSRF protection. The provider
    /// echoes this back in the redirect; callers verify it
    /// matches before using the code.
    pub state: String,
    /// Redirect URI registered for this flow. Callers stand up
    /// a local server at this URL (`http://localhost:<port>/...`)
    /// to catch the code. `anie login` handles this plumbing;
    /// the provider just publishes the URL.
    pub redirect_uri: String,
}

/// Protocol-level OAuth credential, independent of how anie
/// persists it. `AuthCredential::OAuth` is the storage shape;
/// this is the over-the-wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCredentialData {
    /// Bearer token for API requests, valid until `expires_at`.
    pub access_token: String,
    /// Refresh token, used to mint a new access token after
    /// expiry. Rotated on each refresh per Anthropic's flow;
    /// callers MUST persist the new value after every refresh.
    pub refresh_token: String,
    /// RFC 3339 UTC timestamp indicating when `access_token`
    /// expires. pi applies a 5-minute safety margin; we match
    /// pi (see `compute_expires_at`).
    pub expires_at: String,
    /// Account label (usually email). `None` when the provider
    /// doesn't return it at token-exchange time — the field
    /// exists so later UI (e.g. `/providers`) can attribute
    /// credentials to a human-readable identity.
    pub account: Option<String>,
}

/// Shared contract every OAuth provider implements.
///
/// Split into three steps because browser-based OAuth has a
/// mandatory async-with-user phase in the middle that can't be
/// collapsed into one call:
/// 1. `begin_login` — produce a URL, keep PKCE state locally.
/// 2. (Caller: open browser / wait for callback / collect code.)
/// 3. `complete_login` — exchange code for access + refresh.
/// 4. Later, `refresh` — mint a new access token without user
///    interaction.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Human-readable provider identifier (e.g. `"anthropic"`).
    fn name(&self) -> &'static str;

    /// Start an authorization-code + PKCE flow. The returned
    /// `LoginFlow` carries everything needed to finish it.
    async fn begin_login(&self) -> Result<LoginFlow>;

    /// Exchange the authorization code for a credential pair.
    /// `state` comes from the redirect callback and must match
    /// `flow.state` — callers should check this *before*
    /// calling this method. Implementations MAY also verify
    /// defensively.
    async fn complete_login(
        &self,
        flow: &LoginFlow,
        code: &str,
    ) -> Result<OAuthCredentialData>;

    /// Mint a new access + refresh pair from an existing
    /// refresh token. Called by the refresh-with-lock path
    /// (PR C) when the cached token is near expiry.
    async fn refresh(&self, credential: &OAuthCredentialData) -> Result<OAuthCredentialData>;
}

/// PKCE pair for the authorization-code-with-PKCE flow (RFC 7636).
///
/// `verifier` is 32 random bytes → base64url (no padding). The
/// `challenge` is SHA-256(verifier) → base64url. Anthropic
/// expects `code_challenge_method=S256`; we match pi verbatim.
#[derive(Debug, Clone)]
pub struct PkcePair {
    /// Random code_verifier sent at token-exchange time.
    pub verifier: String,
    /// SHA-256 of `verifier`, base64url-encoded, sent at
    /// authorize-request time.
    pub challenge: String,
}

/// Generate a fresh PKCE pair. Uses `rand::rng` for the verifier
/// and `sha2::Sha256` for the challenge.
#[must_use]
pub fn generate_pkce() -> PkcePair {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize());
    PkcePair { verifier, challenge }
}

/// Token-exchange / refresh response shape common to providers
/// that follow RFC 6749 section 5.1. Anthropic returns this
/// shape verbatim; other providers will likely too. Provider-
/// specific quirks (extra fields, alternate names) can deserde
/// into a provider-local struct and convert to this one.
#[derive(Debug, Deserialize)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
}

/// Compute an `expires_at` RFC 3339 string from a provider's
/// `expires_in` seconds value. Applies the same 5-minute safety
/// margin pi does so we treat tokens as expired a bit before
/// the server does — avoids the edge case of sending a request
/// with a token that expires mid-flight.
pub(crate) fn compute_expires_at(expires_in_seconds: u64) -> Result<String> {
    let margin_secs: i64 = 5 * 60;
    let remaining = i64::try_from(expires_in_seconds)
        .map_err(|_| anyhow!("expires_in overflow: {expires_in_seconds}"))?
        .saturating_sub(margin_secs);
    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(remaining);
    expires_at
        .format(&Rfc3339)
        .map_err(|err| anyhow!("failed to format expires_at: {err}"))
}

/// Parse an RFC 3339 `expires_at` back into an `OffsetDateTime`
/// for the expiry check in the refresh path.
pub fn parse_expires_at(expires_at: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(expires_at, &Rfc3339)
        .map_err(|err| anyhow!("invalid expires_at ({expires_at}): {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_pkce_produces_base64url_values_of_expected_length() {
        let pair = generate_pkce();
        // 32 random bytes → 43-char base64url (no padding).
        assert_eq!(pair.verifier.len(), 43, "verifier: {}", pair.verifier);
        // SHA-256 → 32 bytes → also 43 chars base64url.
        assert_eq!(pair.challenge.len(), 43, "challenge: {}", pair.challenge);
        // base64url charset only.
        for ch in pair.verifier.chars().chain(pair.challenge.chars()) {
            assert!(
                ch.is_ascii_alphanumeric() || ch == '-' || ch == '_',
                "non-base64url char: {ch:?}"
            );
        }
    }

    #[test]
    fn generate_pkce_produces_unique_verifiers_on_each_call() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn pkce_challenge_is_sha256_of_verifier() {
        // Verify the challenge is a deterministic function of
        // the verifier, not random. Re-hash the generated
        // verifier and confirm it matches.
        let pair = generate_pkce();
        let mut hasher = Sha256::new();
        hasher.update(pair.verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(hasher.finalize());
        assert_eq!(pair.challenge, expected);
    }

    #[test]
    fn compute_expires_at_applies_five_minute_safety_margin() {
        // 1 hour expiry should produce a timestamp ~55 minutes
        // in the future, not 60.
        let formatted = compute_expires_at(3_600).expect("format");
        let parsed = parse_expires_at(&formatted).expect("parse");
        let now = OffsetDateTime::now_utc();
        let delta = parsed - now;
        let minutes = delta.whole_minutes();
        assert!(
            (53..=57).contains(&minutes),
            "expected ~55 min after safety margin, got {minutes}"
        );
    }

    #[test]
    fn expires_at_roundtrips_through_rfc3339() {
        let formatted = compute_expires_at(3_600).expect("format");
        let parsed = parse_expires_at(&formatted).expect("parse");
        let reformatted = parsed.format(&Rfc3339).expect("reformat");
        assert_eq!(formatted, reformatted);
    }

    #[test]
    fn parse_expires_at_rejects_garbage() {
        assert!(parse_expires_at("not a timestamp").is_err());
    }
}
