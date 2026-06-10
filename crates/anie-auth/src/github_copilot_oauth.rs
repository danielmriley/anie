//! GitHub Copilot OAuth client.
//!
//! Endpoints + client ID + header set verified against pi's
//! `packages/ai/src/utils/oauth/github-copilot.ts` on 2026-04-21.
//!
//! Flow is OAuth 2.0 Device Authorization (RFC 8628) with a
//! second step to exchange the GitHub access token for a
//! Copilot-internal token. In detail:
//!
//! 1. POST `https://github.com/login/device/code` → receive
//!    `device_code`, `user_code`, `verification_uri`, `interval`,
//!    `expires_in`.
//! 2. Display `user_code` + `verification_uri` to the user.
//! 3. Poll `https://github.com/login/oauth/access_token` every
//!    `interval` seconds until the user authorizes in their
//!    browser. Responses: `{access_token: ...}` on success,
//!    `{error: "authorization_pending"}` while waiting, or
//!    `{error: "slow_down", interval: ...}` asking for backoff.
//! 4. Exchange the GitHub OAuth token for a Copilot token at
//!    `https://api.github.com/copilot_internal/v2/token`.
//!    Response: `{token: "tid=...;proxy-ep=proxy.X.githubcopilot.com;...", expires_at: <epoch secs>}`.
//! 5. Extract `proxy-ep` → convert `proxy.X.githubcopilot.com`
//!    to `api.X.githubcopilot.com` and store as
//!    `api_base_url` so the OpenAI-compatible client routes
//!    Copilot requests to the per-user endpoint.
//!
//! Storage shape:
//! - `access_token` = Copilot token (short-lived, ~30 min).
//! - `refresh_token` = GitHub user OAuth token (`ghu_`); we
//!   re-exchange it for a Copilot token on every refresh.
//!   Lifetime depends on the OAuth App's token-expiration
//!   policy — Copilot's client (`Iv1.b507a08c87ecfe98`)
//!   enforces an 8-hour `ghu_` expiry, so without proactive
//!   renewal the credential becomes unusable after the first
//!   day.
//! - `extra_refresh_token` = GitHub refresh token (`ghr_`)
//!   returned alongside the `ghu_` when token expiration is
//!   enabled. Used to mint fresh `ghu_`s via GitHub's
//!   `grant_type=refresh_token` endpoint (~6 month lifetime).
//!   Captured on login (2026-05-08 fix) so refresh can renew
//!   the `ghu_` itself before it expires; absent for OAuth
//!   Apps that don't enforce token expiration, in which case
//!   the existing `ghu_` is treated as long-lived.
//! - `expires_at` = Copilot token's `expires_at` minus pi's
//!   5-minute safety margin.
//! - `refresh_token_expires_at` = `ghu_`'s own expiry, used
//!   by `ensure_fresh_github_user_token` to decide whether
//!   to hit GitHub's refresh endpoint.
//! - `api_base_url` = the `api.*.githubcopilot.com` URL.
//!
//! Enterprise domains (e.g. `github.example-corp.com`) are pi-
//! supported via a custom domain; we ship the `github.com`
//! defaults only. Enterprise support can land as a later PR.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;

use crate::oauth::{DeviceCodeFlow, LoginFlow, OAuthCredentialData, OAuthProvider};

/// Base64-encoded Copilot client ID. Decodes to
/// `Iv1.b507a08c87ecfe98`. Same obfuscation pi uses; not a
/// secret (public-client device flow), just keeps secret-scanner
/// heuristics from flagging it.
const CLIENT_ID_B64: &str = "SXYxLmI1MDdhMDhjODdlY2ZlOTg=";
const DEFAULT_DOMAIN: &str = "github.com";

/// The Copilot token endpoint uses the API subdomain; this
/// prefix joins it.
const COPILOT_API_PREFIX: &str = "api.";

/// Required headers on Copilot-internal token fetches. Pi
/// pins these to specific version strings; we mirror so
/// GitHub's detection doesn't reject as "unknown client".
const UA: &str = "GitHubCopilotChat/0.35.0";
const EDITOR_VERSION: &str = "vscode/1.107.0";
const EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// Pi applies these multipliers to the server-suggested poll
/// interval. Matches pi byte-for-byte so our cadence matches
/// theirs — if GitHub's rate limiter is tuned for pi's cadence,
/// deviating would trigger more slow_down responses.
const INITIAL_POLL_MULTIPLIER: f64 = 1.2;
const SLOW_DOWN_MULTIPLIER: f64 = 1.4;

pub struct GithubCopilotOAuthProvider {
    client: reqwest::Client,
    domain: String,
    // Injectable for tests.
    device_code_url_override: Option<String>,
    access_token_url_override: Option<String>,
    copilot_token_url_override: Option<String>,
}

impl GithubCopilotOAuthProvider {
    /// Production defaults (github.com).
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            domain: DEFAULT_DOMAIN.to_string(),
            device_code_url_override: None,
            access_token_url_override: None,
            copilot_token_url_override: None,
        }
    }

    /// Point every endpoint at the provided base URL. Used by
    /// wiremock tests — the test server hosts all three GitHub
    /// endpoints on the same origin.
    #[cfg(test)]
    fn with_mock_base(base: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            domain: DEFAULT_DOMAIN.to_string(),
            device_code_url_override: Some(format!("{base}/login/device/code")),
            access_token_url_override: Some(format!("{base}/login/oauth/access_token")),
            copilot_token_url_override: Some(format!("{base}/copilot_internal/v2/token")),
        }
    }

    fn client_id() -> Result<String> {
        let bytes = STANDARD
            .decode(CLIENT_ID_B64)
            .map_err(|err| anyhow!("internal: failed to decode client id: {err}"))?;
        String::from_utf8(bytes).map_err(|err| anyhow!("internal: client id not utf-8: {err}"))
    }

    fn device_code_url(&self) -> String {
        self.device_code_url_override
            .clone()
            .unwrap_or_else(|| format!("https://{}/login/device/code", self.domain))
    }

    fn access_token_url(&self) -> String {
        self.access_token_url_override
            .clone()
            .unwrap_or_else(|| format!("https://{}/login/oauth/access_token", self.domain))
    }

    fn copilot_token_url(&self) -> String {
        self.copilot_token_url_override.clone().unwrap_or_else(|| {
            format!(
                "https://{COPILOT_API_PREFIX}{}/copilot_internal/v2/token",
                self.domain
            )
        })
    }

    /// On refresh: if the stored `ghu_` (the GitHub user
    /// token we keep in `refresh_token`) is still well within
    /// its expiry, hand it back as-is. Otherwise — or when an
    /// existing credential predates this code path and has
    /// no `refresh_token_expires_at` recorded — try to mint
    /// a fresh `ghu_` by exchanging the stored `ghr_` (in
    /// `extra_refresh_token`) at GitHub's
    /// `grant_type=refresh_token` endpoint. If the
    /// credential has no `extra_refresh_token` (legacy auth
    /// files written before this fix landed, or OAuth Apps
    /// that don't enforce token expiration), we fall through
    /// to using the existing `ghu_` and let the Copilot
    /// re-exchange surface any subsequent failure.
    async fn ensure_fresh_github_user_token(
        &self,
        credential: &OAuthCredentialData,
    ) -> Result<GithubUserToken> {
        let near_expiry = credential
            .refresh_token_expires_at
            .as_deref()
            .map(|expires_at| github_user_token_near_expiry(expires_at).unwrap_or(true))
            .unwrap_or(false);
        if !near_expiry {
            return Ok(GithubUserToken::passthrough(credential));
        }
        let Some(github_refresh_token) = credential.extra_refresh_token.as_deref() else {
            // No second-tier refresh token to exchange. Pass
            // the existing `ghu_` through; the Copilot
            // re-exchange will fail with HTTP 401 and the
            // user will see the standard refresh-failed
            // breadcrumb prompting them to re-`/login`.
            return Ok(GithubUserToken::passthrough(credential));
        };
        let response = self.refresh_github_user_token(github_refresh_token).await?;
        Ok(GithubUserToken {
            access_token: response.access_token,
            expires_in: response.expires_in,
            // Per RFC 6749 §6, the response MAY include a
            // new refresh token; when it does we rotate. If
            // it doesn't, keep the existing `ghr_`.
            extra_refresh_token: response
                .refresh_token
                .or_else(|| Some(github_refresh_token.to_string())),
            // We just refreshed, so the existing
            // `refresh_token_expires_at` is stale. Setting
            // this to `None` lets `build_credential` recompute
            // from the new `expires_in`.
            refresh_token_expires_at: None,
        })
    }

    /// POST `https://github.com/login/oauth/access_token`
    /// with `grant_type=refresh_token` to get a new `ghu_`
    /// from the stored `ghr_`. Sets the `Accept: application/json`
    /// header so GitHub returns JSON (the default is
    /// form-urlencoded which is harder to parse here).
    async fn refresh_github_user_token(
        &self,
        github_refresh_token: &str,
    ) -> Result<RefreshedGithubUserToken> {
        let client_id = Self::client_id()?;
        let response = self
            .client
            .post(self.access_token_url())
            .header("Accept", "application/json")
            .header("User-Agent", UA)
            .form(&[
                ("client_id", client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", github_refresh_token),
            ])
            .send()
            .await
            .map_err(|err| anyhow!("github user-token refresh request failed: {err}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| anyhow!("failed to read github user-token refresh body: {err}"))?;
        if !status.is_success() {
            return Err(anyhow!(
                "github user-token refresh returned HTTP {status} (body: {text})"
            ));
        }
        let parsed: AccessTokenResponse = serde_json::from_str(&text).map_err(|err| {
            anyhow!(
                "github user-token refresh body did not parse as JSON ({err}); body was: {text}"
            )
        })?;
        match parsed {
            AccessTokenResponse::Success {
                access_token,
                expires_in,
                refresh_token,
                ..
            } => Ok(RefreshedGithubUserToken {
                access_token,
                expires_in,
                refresh_token,
            }),
            AccessTokenResponse::Error {
                error,
                error_description,
                ..
            } => {
                let suffix = error_description
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default();
                Err(anyhow!(
                    "github user-token refresh rejected: {error}{suffix}"
                ))
            }
        }
    }

    /// Step 4: exchange the GitHub OAuth access_token for a
    /// Copilot token. Called both at login completion and on
    /// every refresh.
    async fn fetch_copilot_token(&self, github_access_token: &str) -> Result<CopilotTokenResponse> {
        let response = self
            .client
            .get(self.copilot_token_url())
            .bearer_auth(github_access_token)
            .header("Accept", "application/json")
            .header("User-Agent", UA)
            .header("Editor-Version", EDITOR_VERSION)
            .header("Editor-Plugin-Version", EDITOR_PLUGIN_VERSION)
            .header("Copilot-Integration-Id", COPILOT_INTEGRATION_ID)
            .send()
            .await
            .map_err(|err| anyhow!("copilot token request failed: {err}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| anyhow!("failed to read copilot token response body: {err}"))?;
        if !status.is_success() {
            return Err(anyhow!(
                "copilot token endpoint returned HTTP {status} (body: {text})"
            ));
        }
        serde_json::from_str(&text).map_err(|err| {
            anyhow!("copilot token response did not parse as JSON ({err}); body was: {text}")
        })
    }
}

impl Default for GithubCopilotOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    interval: u64,
    expires_in: u64,
}

/// Token endpoint response — either success (access_token) or
/// an error body. We deserialize into the untagged enum so one
/// pass handles both shapes.
///
/// The `Success` variant captures every field GitHub returns
/// when token expiration is enabled on the OAuth App: a
/// short-lived `ghu_` (~8 h, in `access_token` + `expires_in`)
/// and a long-lived `ghr_` (~6 mo, in `refresh_token` +
/// `refresh_token_expires_in`). The Copilot client
/// (`Iv1.b507a08c87ecfe98`) has token expiration enabled, so
/// without capturing the `ghr_` we'd be stuck after the first
/// 8-hour window. All four expiration-related fields are
/// optional with serde defaults so OAuth Apps without token
/// expiration (which return only `access_token` + scope) still
/// parse cleanly — those credentials simply leave
/// `extra_refresh_token` `None` and fall back to the
/// pre-fix behaviour of treating `access_token` as long-lived.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AccessTokenResponse {
    Success {
        access_token: String,
        #[allow(dead_code)]
        #[serde(default)]
        token_type: Option<String>,
        #[allow(dead_code)]
        #[serde(default)]
        scope: Option<String>,
        /// Lifetime of `access_token` in seconds. Present
        /// when the OAuth App has token expiration enabled.
        #[serde(default)]
        expires_in: Option<u64>,
        /// Refresh token used to obtain a new
        /// `access_token` after `expires_in` elapses.
        /// Present when the OAuth App has token expiration
        /// enabled.
        #[serde(default)]
        refresh_token: Option<String>,
        /// Lifetime of `refresh_token` itself in seconds.
        /// GitHub's default is 6 months for the Copilot
        /// client. Currently unused (we trust GitHub's
        /// own enforcement and surface the failure on the
        /// next refresh call) but captured for symmetry +
        /// future proactive-warning UI.
        #[allow(dead_code)]
        #[serde(default)]
        refresh_token_expires_in: Option<u64>,
    },
    Error {
        error: String,
        #[serde(default)]
        error_description: Option<String>,
        #[serde(default)]
        interval: Option<u64>,
    },
}

/// Result of the device-flow access-token poll. Carries the
/// short-lived GitHub user token plus, when GitHub's OAuth
/// App has token expiration enabled, its expiry and the
/// `ghr_` refresh token used to renew it.
struct DeviceFlowOutcome {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// Output of `ensure_fresh_github_user_token`. Either the
/// existing `ghu_` passed through unchanged (when not yet
/// near-expiry, or when there's no `ghr_` available to
/// refresh from), or a freshly-minted one from GitHub's
/// `grant_type=refresh_token` endpoint.
struct GithubUserToken {
    access_token: String,
    expires_in: Option<u64>,
    extra_refresh_token: Option<String>,
    /// `Some(existing)` when we passed the existing `ghu_`
    /// through unchanged — the caller carries it forward.
    /// `None` when we just refreshed; the caller recomputes
    /// from `expires_in`.
    refresh_token_expires_at: Option<String>,
}

impl GithubUserToken {
    fn passthrough(credential: &OAuthCredentialData) -> Self {
        Self {
            access_token: credential.refresh_token.clone(),
            // We don't know the original `expires_in` (we only
            // stored `refresh_token_expires_at`); leave None
            // so `build_credential` doesn't overwrite the
            // existing `refresh_token_expires_at`.
            expires_in: None,
            extra_refresh_token: credential.extra_refresh_token.clone(),
            refresh_token_expires_at: credential.refresh_token_expires_at.clone(),
        }
    }
}

/// Successful body of GitHub's `grant_type=refresh_token`
/// response. The refresh-token field may or may not be
/// present per RFC 6749 §6.
struct RefreshedGithubUserToken {
    access_token: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

/// True when the stored `refresh_token_expires_at` is within
/// the same 30-second margin the [`crate::refresh`] broker
/// uses for the access token. Errors during parsing are
/// treated as "yes, near expiry" so we attempt a refresh
/// rather than silently using a possibly-stale token.
fn github_user_token_near_expiry(expires_at: &str) -> Result<bool> {
    let deadline =
        time::OffsetDateTime::parse(expires_at, &time::format_description::well_known::Rfc3339)
            .map_err(|err| anyhow!("invalid refresh_token_expires_at: {err}"))?;
    let now = time::OffsetDateTime::now_utc();
    Ok(deadline - now <= time::Duration::seconds(30))
}

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: String,
    /// Epoch seconds (NOT expires_in). Pi treats it verbatim;
    /// we convert to RFC 3339 with the 5-minute safety margin.
    expires_at: i64,
}

#[async_trait]
impl OAuthProvider for GithubCopilotOAuthProvider {
    fn name(&self) -> &'static str {
        "github-copilot"
    }

    async fn begin_login(&self) -> Result<LoginFlow> {
        let client_id = Self::client_id()?;
        let form = [("client_id", client_id.as_str()), ("scope", "read:user")];
        let response = self
            .client
            .post(self.device_code_url())
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", UA)
            .form(&form)
            .send()
            .await
            .map_err(|err| anyhow!("device-code request failed: {err}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| anyhow!("failed to read device-code response body: {err}"))?;
        if !status.is_success() {
            return Err(anyhow!(
                "device-code endpoint returned HTTP {status} (body: {text})"
            ));
        }
        let resp: DeviceCodeResponse = serde_json::from_str(&text).map_err(|err| {
            anyhow!("device-code response did not parse as JSON ({err}); body was: {text}")
        })?;

        Ok(LoginFlow::Device(DeviceCodeFlow {
            user_code: resp.user_code,
            verification_uri: resp.verification_uri,
            verification_uri_complete: resp.verification_uri_complete,
            device_code: resp.device_code,
            interval: Duration::from_secs(resp.interval.max(1)),
            expires_in: Duration::from_secs(resp.expires_in),
        }))
    }

    async fn complete_login(
        &self,
        flow: &LoginFlow,
        _code: Option<&str>,
    ) -> Result<OAuthCredentialData> {
        let device = flow.as_device().ok_or_else(|| {
            anyhow!("GitHub Copilot uses device-code flow; auth-code flow not supported")
        })?;

        let outcome = poll_device_flow(
            &self.client,
            &self.access_token_url(),
            &Self::client_id()?,
            device,
        )
        .await
        .context("device-flow polling failed")?;

        let copilot = self
            .fetch_copilot_token(&outcome.access_token)
            .await
            .context("copilot token exchange failed")?;

        Ok(build_credential(
            outcome.access_token,
            outcome.expires_in,
            outcome.refresh_token,
            copilot,
        )?)
    }

    async fn refresh(&self, credential: &OAuthCredentialData) -> Result<OAuthCredentialData> {
        // Two-layer refresh:
        //
        // 1. If the GitHub user token (the `ghu_` we store as
        //    `refresh_token`) is itself near-expiry AND we
        //    have a `ghr_` from the device flow, exchange the
        //    `ghr_` at GitHub's `grant_type=refresh_token`
        //    endpoint to get a fresh `ghu_`. Without this,
        //    OAuth Apps with token expiration (the Copilot
        //    client included) leave us holding an expired
        //    `ghu_` after ~8 hours and the next step fails
        //    with HTTP 401 — the user gets booted to
        //    re-`/login` even though their `ghr_` is good
        //    for ~6 months.
        //
        // 2. Re-exchange the (possibly fresh) `ghu_` for a
        //    new Copilot token, same call as step 4 of login.
        let github_access_token = self
            .ensure_fresh_github_user_token(credential)
            .await
            .context("github user-token refresh failed")?;
        let inherited_extra_refresh = github_access_token.extra_refresh_token.clone();
        let inherited_refresh_expires_at = github_access_token.refresh_token_expires_at.clone();
        let copilot = self
            .fetch_copilot_token(&github_access_token.access_token)
            .await
            .context("copilot token re-exchange failed")?;
        let mut next = build_credential(
            github_access_token.access_token,
            github_access_token.expires_in,
            inherited_extra_refresh,
            copilot,
        )?;
        // Carry over `refresh_token_expires_at` if the inner
        // helper preserved it (i.e., we did NOT actually
        // refresh the `ghu_` this round and the existing
        // expiry timestamp is still authoritative).
        if next.refresh_token_expires_at.is_none() {
            next.refresh_token_expires_at = inherited_refresh_expires_at;
        }
        // Refresh path preserves the account label (if any)
        // since the Copilot token response doesn't re-emit it.
        next.account = credential.account.clone();
        Ok(next)
    }
}

/// Poll the token endpoint per RFC 8628. Respects
/// `interval`, handles `authorization_pending` (keep waiting)
/// and `slow_down` (back off), and gives up when the
/// `expires_in` deadline passes.
async fn poll_device_flow(
    client: &reqwest::Client,
    access_token_url: &str,
    client_id: &str,
    device: &DeviceCodeFlow,
) -> Result<DeviceFlowOutcome> {
    let deadline = tokio::time::Instant::now() + device.expires_in;
    let mut interval_ms = (device.interval.as_millis() as f64).max(1000.0);
    let mut multiplier = INITIAL_POLL_MULTIPLIER;
    let mut saw_slow_down = false;

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            let msg = if saw_slow_down {
                "device flow timed out after slow_down responses — check system clock drift (WSL / VM)"
            } else {
                "device flow timed out waiting for authorization"
            };
            return Err(anyhow!(msg));
        }
        let remaining = deadline - now;
        let wait_ms = (interval_ms * multiplier).min(remaining.as_millis() as f64);
        tokio::time::sleep(Duration::from_millis(wait_ms as u64)).await;

        let response = client
            .post(access_token_url)
            .header("Accept", "application/json")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", UA)
            .form(&[
                ("client_id", client_id),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|err| anyhow!("access-token poll failed: {err}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| anyhow!("failed to read access-token poll body: {err}"))?;
        // Note: GitHub returns HTTP 200 even for the pending /
        // slow_down cases, with the discriminator in the body.
        if !status.is_success() && status.as_u16() != 200 {
            return Err(anyhow!(
                "access-token poll returned HTTP {status} (body: {text})"
            ));
        }
        let parsed: AccessTokenResponse = serde_json::from_str(&text).map_err(|err| {
            anyhow!("access-token poll did not parse as JSON ({err}); body was: {text}")
        })?;

        match parsed {
            AccessTokenResponse::Success {
                access_token,
                expires_in,
                refresh_token,
                ..
            } => {
                return Ok(DeviceFlowOutcome {
                    access_token,
                    expires_in,
                    refresh_token,
                });
            }
            AccessTokenResponse::Error {
                error,
                error_description,
                interval,
            } => match error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    saw_slow_down = true;
                    if let Some(server_interval) = interval {
                        interval_ms = (server_interval * 1000) as f64;
                    } else {
                        // Fallback: +5s per pi's behavior.
                        interval_ms = (interval_ms + 5_000.0).max(1_000.0);
                    }
                    multiplier = SLOW_DOWN_MULTIPLIER;
                }
                other => {
                    let suffix = error_description
                        .map(|d| format!(": {d}"))
                        .unwrap_or_default();
                    return Err(anyhow!("device flow rejected: {other}{suffix}"));
                }
            },
        }
    }
}

/// Convert a successful Copilot token response into the
/// credential shape anie-auth stores. `github_user_token` is
/// the `ghu_` we use as `refresh_token`; the
/// `github_user_expires_in` and `github_refresh_token` (the
/// `ghr_`) parameters come from GitHub's device-flow response
/// when token expiration is enabled on the OAuth App. When the
/// `ghu_` is long-lived (no `expires_in` returned),
/// `refresh_token_expires_at` and `extra_refresh_token` stay
/// `None` and the refresh broker falls back to the pre-fix
/// behaviour of treating the `ghu_` as never expiring.
fn build_credential(
    github_user_token: String,
    github_user_expires_in: Option<u64>,
    github_refresh_token: Option<String>,
    copilot: CopilotTokenResponse,
) -> Result<OAuthCredentialData> {
    let api_base_url = extract_api_base_url(&copilot.token);
    let expires_at = format_expires_at_from_epoch(copilot.expires_at)?;
    let refresh_token_expires_at = match github_user_expires_in {
        Some(seconds) => Some(format_expires_at_from_seconds_from_now(seconds)?),
        None => None,
    };
    Ok(OAuthCredentialData {
        access_token: copilot.token,
        refresh_token: github_user_token,
        expires_at,
        account: None,
        api_base_url,
        project_id: None,
        refresh_token_expires_at,
        extra_refresh_token: github_refresh_token,
    })
}

/// RFC 3339 timestamp `seconds` from now, with the same
/// 5-minute safety margin we apply to the Copilot token
/// expiry. The deadline used by `is_near_expiry` ends up
/// `seconds - 300` from now.
fn format_expires_at_from_seconds_from_now(seconds: u64) -> Result<String> {
    let safety_margin: u64 = 5 * 60;
    let adjusted = seconds.saturating_sub(safety_margin);
    let dt = time::OffsetDateTime::now_utc()
        + time::Duration::seconds(i64::try_from(adjusted).unwrap_or(0));
    dt.format(&time::format_description::well_known::Rfc3339)
        .map_err(|err| anyhow!("failed to format refresh-token expires_at: {err}"))
}

/// Extract the `proxy-ep` field out of the Copilot token
/// string and rewrite `proxy.*` → `api.*` to form the user's
/// API base URL. Returns `None` when the field is absent
/// (shouldn't happen on a healthy token, but we don't want to
/// break login if GitHub omits it).
fn extract_api_base_url(token: &str) -> Option<String> {
    for segment in token.split(';') {
        let segment = segment.trim();
        if let Some(value) = segment.strip_prefix("proxy-ep=") {
            let api_host = value
                .strip_prefix("proxy.")
                .map(|rest| format!("api.{rest}"))
                .unwrap_or_else(|| value.to_string());
            return Some(format!("https://{api_host}"));
        }
    }
    None
}

/// Copilot returns epoch seconds; we store RFC 3339 with a
/// 5-minute safety margin so refresh fires before the
/// provider's wall-clock expiry.
fn format_expires_at_from_epoch(epoch_secs: i64) -> Result<String> {
    let safety_margin: i64 = 5 * 60;
    let adjusted = epoch_secs.saturating_sub(safety_margin);
    let dt = time::OffsetDateTime::from_unix_timestamp(adjusted)
        .map_err(|err| anyhow!("invalid copilot expires_at: {err}"))?;
    dt.format(&time::format_description::well_known::Rfc3339)
        .map_err(|err| anyhow!("failed to format expires_at: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path as match_path},
    };

    const SAMPLE_COPILOT_TOKEN: &str =
        "tid=abc;exp=9999999999;sku=free;proxy-ep=proxy.individual.githubcopilot.com;u=user";

    fn device_code_body() -> serde_json::Value {
        serde_json::json!({
            "device_code": "dev-code-123",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "interval": 1,
            "expires_in": 900,
        })
    }

    fn copilot_token_body() -> serde_json::Value {
        // `expires_at`: 2099-12-31 so the test's parse
        // produces a stable future timestamp.
        serde_json::json!({
            "token": SAMPLE_COPILOT_TOKEN,
            "expires_at": 4_102_444_800_i64,
        })
    }

    #[test]
    fn client_id_decodes_to_expected_string() {
        assert_eq!(
            GithubCopilotOAuthProvider::client_id().expect("decode"),
            "Iv1.b507a08c87ecfe98",
        );
    }

    #[test]
    fn extract_api_base_url_rewrites_proxy_to_api() {
        let url = extract_api_base_url(SAMPLE_COPILOT_TOKEN);
        assert_eq!(
            url.as_deref(),
            Some("https://api.individual.githubcopilot.com")
        );
    }

    #[test]
    fn extract_api_base_url_handles_missing_proxy_ep() {
        assert!(extract_api_base_url("tid=abc;exp=1;sku=free").is_none());
    }

    #[test]
    fn extract_api_base_url_leaves_non_proxy_prefix_alone() {
        // Defensive: if GitHub ever publishes a non-proxy
        // endpoint, we should preserve it verbatim.
        let token = "tid=abc;proxy-ep=custom.githubcopilot.com;";
        assert_eq!(
            extract_api_base_url(token).as_deref(),
            Some("https://custom.githubcopilot.com"),
        );
    }

    #[test]
    fn format_expires_at_from_epoch_applies_safety_margin() {
        let formatted = format_expires_at_from_epoch(4_102_444_800).expect("format");
        // 5-minute margin → 4_102_444_500
        let parsed = crate::oauth::parse_expires_at(&formatted).expect("parse");
        assert_eq!(parsed.unix_timestamp(), 4_102_444_500);
    }

    #[tokio::test]
    async fn begin_login_returns_device_flow_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_code_body()))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let device = flow.as_device().expect("device flow");
        assert_eq!(device.user_code, "ABCD-1234");
        assert_eq!(device.device_code, "dev-code-123");
        assert_eq!(device.verification_uri, "https://github.com/login/device");
        assert_eq!(device.interval, Duration::from_secs(1));
    }

    #[tokio::test]
    async fn complete_login_polls_then_exchanges_for_copilot_token() {
        let server = MockServer::start().await;

        // First: device code dispatch.
        Mock::given(method("POST"))
            .and(match_path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_code_body()))
            .mount(&server)
            .await;

        // Then: poll returns authorization_pending once, then success.
        // wiremock's default mount order matters — the newer
        // mount wins on priority. We install a `up_to_n_times`
        // pending response + an unbounded success response.
        Mock::given(method("POST"))
            .and(match_path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"error": "authorization_pending"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(match_path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "github-oauth-token"})),
            )
            .mount(&server)
            .await;

        // Copilot-internal exchange.
        Mock::given(method("GET"))
            .and(match_path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(copilot_token_body()))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, None)
            .await
            .expect("complete_login");
        assert_eq!(cred.access_token, SAMPLE_COPILOT_TOKEN);
        assert_eq!(cred.refresh_token, "github-oauth-token");
        assert_eq!(
            cred.api_base_url.as_deref(),
            Some("https://api.individual.githubcopilot.com"),
        );
        // Device-flow body in this test omits expires_in /
        // refresh_token; the new optional fields stay None
        // and the credential falls through to the legacy
        // long-lived-ghu_ behaviour.
        assert!(cred.refresh_token_expires_at.is_none());
        assert!(cred.extra_refresh_token.is_none());
    }

    /// 2026-05-08 fix: when GitHub's device-flow response
    /// includes `expires_in` + `refresh_token` (the modern
    /// shape returned for OAuth Apps with token expiration —
    /// which the Copilot client uses), `complete_login`
    /// captures both into the persisted credential. This is
    /// the load-bearing path that lets `refresh()` later
    /// renew the `ghu_` instead of getting stuck after
    /// 8 hours.
    #[tokio::test]
    async fn complete_login_captures_ghr_when_github_returns_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_code_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(match_path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "ghu_initial",
                "expires_in": 28_800u64,
                "refresh_token": "ghr_initial",
                "refresh_token_expires_in": 15_897_600u64,
                "token_type": "bearer",
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(copilot_token_body()))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, None)
            .await
            .expect("complete_login");

        assert_eq!(cred.refresh_token, "ghu_initial");
        assert_eq!(cred.extra_refresh_token.as_deref(), Some("ghr_initial"));
        assert!(
            cred.refresh_token_expires_at.is_some(),
            "expected the 8-hour ghu_ expiry to be recorded"
        );
    }

    #[tokio::test]
    async fn refresh_re_exchanges_github_token_for_new_copilot_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(copilot_token_body()))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let prior = OAuthCredentialData {
            access_token: "old-copilot-token".into(),
            refresh_token: "persisted-github-oauth".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("octocat".into()),
            api_base_url: Some("https://api.individual.githubcopilot.com".into()),
            project_id: None,
            refresh_token_expires_at: None,
            extra_refresh_token: None,
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        assert_eq!(refreshed.access_token, SAMPLE_COPILOT_TOKEN);
        // Refresh token (GitHub OAuth) unchanged — we reuse it.
        assert_eq!(refreshed.refresh_token, "persisted-github-oauth");
        // Account carries forward.
        assert_eq!(refreshed.account.as_deref(), Some("octocat"));
    }

    /// 2026-05-08 fix: when the stored `ghu_` (in
    /// `refresh_token`) is itself near-expiry AND we have a
    /// `ghr_` (in `extra_refresh_token`), the refresh path
    /// must hit GitHub's `grant_type=refresh_token` endpoint
    /// to get a fresh `ghu_` BEFORE re-exchanging for a
    /// Copilot token. Without this, OAuth Apps with token
    /// expiration leave us stuck after ~8 hours.
    ///
    /// Setup: `ghu_` "expired-ghu" past expiry, `ghr_`
    /// "good-ghr" still valid. Mock GitHub's refresh endpoint
    /// to return a fresh `ghu_` ("fresh-ghu") + a rotated
    /// `ghr_` ("rotated-ghr"). Mock the Copilot token
    /// endpoint to expect the FRESH `ghu_` as the bearer.
    #[tokio::test]
    async fn refresh_renews_ghu_via_grant_type_refresh_token_when_near_expiry() {
        use wiremock::matchers::{body_string_contains, header};
        let server = MockServer::start().await;

        // GitHub refresh endpoint returns a new ghu_ + rotated ghr_.
        Mock::given(method("POST"))
            .and(match_path("/login/oauth/access_token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=good-ghr"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fresh-ghu",
                "expires_in": 28_800u64,
                "refresh_token": "rotated-ghr",
                "refresh_token_expires_in": 15_897_600u64,
                "token_type": "bearer",
            })))
            .mount(&server)
            .await;

        // Copilot token endpoint must see the fresh ghu_ (not
        // the expired one). Mounting with a header matcher
        // catches the regression where we'd accidentally
        // pass the expired token through.
        Mock::given(method("GET"))
            .and(match_path("/copilot_internal/v2/token"))
            .and(header("Authorization", "Bearer fresh-ghu"))
            .respond_with(ResponseTemplate::new(200).set_body_json(copilot_token_body()))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let prior = OAuthCredentialData {
            access_token: "old-copilot".into(),
            refresh_token: "expired-ghu".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("octocat".into()),
            api_base_url: Some("https://api.individual.githubcopilot.com".into()),
            project_id: None,
            // `ghu_` recorded as already past expiry.
            refresh_token_expires_at: Some("2026-04-20T00:00:00Z".into()),
            extra_refresh_token: Some("good-ghr".into()),
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");

        assert_eq!(refreshed.access_token, SAMPLE_COPILOT_TOKEN);
        // Stored `refresh_token` rotated to the fresh ghu_.
        assert_eq!(refreshed.refresh_token, "fresh-ghu");
        // Stored `extra_refresh_token` rotated to the new ghr_.
        assert_eq!(
            refreshed.extra_refresh_token.as_deref(),
            Some("rotated-ghr")
        );
        // New refresh-token expiry recorded (5-min safety
        // margin so deadline is `now + 28800 - 300 ≈ 28500s`).
        assert!(refreshed.refresh_token_expires_at.is_some());
    }

    /// Forward-compat: a credential with no
    /// `extra_refresh_token` (i.e., one written before this
    /// fix landed, OR an OAuth App that doesn't enforce
    /// token expiration) must still refresh successfully —
    /// it just falls through to the original "treat
    /// `refresh_token` as long-lived" path. The Copilot
    /// re-exchange uses the existing `ghu_` directly.
    #[tokio::test]
    async fn refresh_falls_through_when_extra_refresh_token_is_missing() {
        use wiremock::matchers::header;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(match_path("/copilot_internal/v2/token"))
            .and(header("Authorization", "Bearer legacy-ghu"))
            .respond_with(ResponseTemplate::new(200).set_body_json(copilot_token_body()))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let prior = OAuthCredentialData {
            access_token: "old-copilot".into(),
            refresh_token: "legacy-ghu".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("octocat".into()),
            api_base_url: Some("https://api.individual.githubcopilot.com".into()),
            project_id: None,
            refresh_token_expires_at: None,
            extra_refresh_token: None,
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        assert_eq!(refreshed.access_token, SAMPLE_COPILOT_TOKEN);
        // ghu_ unchanged — no refresh-token endpoint hit.
        assert_eq!(refreshed.refresh_token, "legacy-ghu");
    }

    #[tokio::test]
    async fn device_flow_surfaces_unexpected_error_codes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_code_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(match_path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": "expired_token",
                "error_description": "user took too long"
            })))
            .mount(&server)
            .await;

        let provider = GithubCopilotOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let err = format!(
            "{:#}",
            provider.complete_login(&flow, None).await.unwrap_err()
        );
        assert!(err.contains("expired_token"), "{err}");
        assert!(err.contains("user took too long"), "{err}");
    }
}
