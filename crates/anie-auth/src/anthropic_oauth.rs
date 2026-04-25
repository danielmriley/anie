//! Anthropic-specific OAuth client (Claude Pro / Max login).
//!
//! Endpoints + client ID + scopes verified against pi's
//! `packages/ai/src/utils/oauth/anthropic.ts` on 2026-04-21.
//! If Anthropic changes the flow, pi's file is the reference
//! point — check there first, update here, re-run the wiremock
//! tests, and bump the verification date.
//!
//! The flow is OAuth 2.0 Authorization Code + PKCE (RFC 7636,
//! S256). Steps:
//!
//! 1. `begin_login` builds the `https://claude.ai/oauth/authorize`
//!    URL with `code_challenge` + random `state` (we use the
//!    PKCE `verifier` as state, same as pi).
//! 2. User opens URL in browser, logs in, Anthropic redirects
//!    to `http://localhost:53692/callback?code=...&state=...`.
//!    anie's CLI / TUI stands up the local server (PR D).
//! 3. `complete_login` POSTs the code + verifier to
//!    `https://platform.claude.com/v1/oauth/token` → receives
//!    `{access_token, refresh_token, expires_in}`.
//! 4. `refresh` POSTs `{grant_type=refresh_token, refresh_token}`
//!    to the same token URL. Anthropic issues a new refresh
//!    token each time — we persist the new value.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};

use crate::oauth::{
    AuthCodeFlow, LoginFlow, OAuthCredentialData, OAuthProvider, PkcePair, TokenResponse,
    compute_expires_at, generate_pkce,
};

/// Anthropic's public Claude Code OAuth client ID. Base64-
/// encoded here purely to match pi's convention — not a secret
/// (public-client OAuth), just keeps bulk-grep / secret-scanner
/// tooling from flagging it.
///
/// Decodes to `9d1c250a-e61b-44d9-88ed-5944d1962f5e`.
const CLIENT_ID_B64: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";

/// Anthropic endpoints (verified 2026-04-21 against pi's
/// anthropic.ts). See the module doc for the refresh cadence.
const DEFAULT_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// Callback server port + path pi registered. Fixed value so
/// the redirect URI matches what Anthropic expects.
const CALLBACK_PORT: u16 = 53692;
const CALLBACK_PATH: &str = "/callback";

/// OAuth scopes pi requests. `user:inference` is the key one
/// that lets Claude Code use the API; the rest cover profile +
/// session management.
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Anthropic OAuth provider implementation.
///
/// Hold onto one instance for the lifetime of the process. The
/// internal `reqwest::Client` is reused so connection-pooling
/// works across retries.
pub struct AnthropicOAuthProvider {
    client: reqwest::Client,
    authorize_url: String,
    token_url: String,
    redirect_uri: String,
}

impl AnthropicOAuthProvider {
    /// Use Anthropic's production endpoints. This is the
    /// constructor CLI / TUI callers want.
    #[must_use]
    pub fn new() -> Self {
        Self::with_endpoints(
            DEFAULT_AUTHORIZE_URL.to_string(),
            DEFAULT_TOKEN_URL.to_string(),
            default_redirect_uri(),
        )
    }

    /// Construct with explicit endpoints. Tests use this to
    /// point the provider at a wiremock server; production
    /// code calls `new()`.
    #[must_use]
    pub fn with_endpoints(authorize_url: String, token_url: String, redirect_uri: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            authorize_url,
            token_url,
            redirect_uri,
        }
    }

    fn client_id() -> Result<String> {
        let bytes = STANDARD
            .decode(CLIENT_ID_B64)
            .map_err(|err| anyhow!("internal: failed to decode client id: {err}"))?;
        String::from_utf8(bytes).map_err(|err| anyhow!("internal: client id not utf-8: {err}"))
    }
}

impl Default for AnthropicOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn default_redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

#[async_trait]
impl OAuthProvider for AnthropicOAuthProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn begin_login(&self) -> Result<LoginFlow> {
        let PkcePair {
            verifier,
            challenge,
        } = generate_pkce();
        let client_id = Self::client_id()?;

        // Build the authorize URL. Matches pi's anthropic.ts:
        // - `code=true` query flag (Anthropic-specific).
        // - `state` = the PKCE verifier; we echo-check it on the
        //   callback. This is unusual (spec normally says state
        //   should be independent random), but pi does it this
        //   way and it works.
        let params = [
            ("code", "true"),
            ("client_id", client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", SCOPES),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", verifier.as_str()),
        ];
        let query = serde_urlencoded::to_string(params)
            .map_err(|err| anyhow!("failed to encode authorize params: {err}"))?;
        let authorize_url = format!("{}?{query}", self.authorize_url);

        Ok(LoginFlow::AuthorizationCode(AuthCodeFlow {
            authorize_url,
            state: verifier.clone(),
            verifier,
            redirect_uri: self.redirect_uri.clone(),
        }))
    }

    async fn complete_login(
        &self,
        flow: &LoginFlow,
        code: Option<&str>,
    ) -> Result<OAuthCredentialData> {
        let flow = flow.as_auth_code().ok_or_else(|| {
            anyhow!("Anthropic uses authorization-code flow; device flow not supported")
        })?;
        let code =
            code.ok_or_else(|| anyhow!("Anthropic login requires the authorization code"))?;
        let client_id = Self::client_id()?;
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": client_id,
            "code": code,
            "state": flow.state,
            "redirect_uri": flow.redirect_uri,
            "code_verifier": flow.verifier,
        });
        let response: TokenResponse = post_token_request(&self.client, &self.token_url, &body)
            .await
            .context("authorization_code exchange failed")?;
        Ok(OAuthCredentialData::new(
            response.access_token,
            response.refresh_token,
            compute_expires_at(response.expires_in)?,
        ))
    }

    async fn refresh(&self, credential: &OAuthCredentialData) -> Result<OAuthCredentialData> {
        let client_id = Self::client_id()?;
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": client_id,
            "refresh_token": credential.refresh_token,
        });
        let response: TokenResponse = post_token_request(&self.client, &self.token_url, &body)
            .await
            .context("refresh_token exchange failed")?;
        Ok(OAuthCredentialData {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: compute_expires_at(response.expires_in)?,
            account: credential.account.clone(),
            api_base_url: credential.api_base_url.clone(),
            project_id: credential.project_id.clone(),
        })
    }
}

/// Post a JSON body to `url`, expect a JSON response, parse as
/// `TokenResponse`. On HTTP error, include the response body in
/// the error message so provider-side failures (rate limit,
/// invalid grant, etc.) surface with enough context to debug.
async fn post_token_request(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<TokenResponse> {
    let response = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(body)
        .send()
        .await
        .map_err(|err| anyhow!("token request failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| anyhow!("failed to read token response body: {err}"))?;
    if !status.is_success() {
        return Err(anyhow!(
            "token endpoint returned HTTP {status} (body: {text})"
        ));
    }
    serde_json::from_str::<TokenResponse>(&text)
        .map_err(|err| anyhow!("token response did not parse as JSON ({err}); body was: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, method, path},
    };

    fn sample_response() -> serde_json::Value {
        serde_json::json!({
            "access_token": "sk-ant-oat01-new",
            "refresh_token": "sk-ant-ort01-rotated",
            "expires_in": 3_600,
        })
    }

    #[test]
    fn client_id_decodes_to_expected_uuid() {
        let id = AnthropicOAuthProvider::client_id().expect("decode");
        assert_eq!(id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    }

    #[test]
    fn default_redirect_uri_matches_pi_callback_port() {
        // Regression: if this port ever changes, Anthropic's
        // registered callback breaks. Keep it pinned.
        assert_eq!(default_redirect_uri(), "http://localhost:53692/callback");
    }

    #[tokio::test]
    async fn begin_login_produces_a_valid_authorize_url() {
        let provider = AnthropicOAuthProvider::new();
        let flow = provider.begin_login().await.expect("begin_login");
        let auth_code = flow
            .as_auth_code()
            .expect("anthropic must use auth-code flow");
        assert!(
            auth_code.authorize_url.starts_with(DEFAULT_AUTHORIZE_URL),
            "{}",
            auth_code.authorize_url
        );
        // Required query params are present.
        for key in [
            "code=true",
            "client_id=",
            "response_type=code",
            "redirect_uri=",
            "scope=",
            "code_challenge=",
            "code_challenge_method=S256",
            "state=",
        ] {
            assert!(
                auth_code.authorize_url.contains(key),
                "missing {key} in: {}",
                auth_code.authorize_url
            );
        }
        // state == verifier per pi's convention.
        assert_eq!(auth_code.state, auth_code.verifier);
    }

    #[tokio::test]
    async fn complete_login_posts_expected_grant_and_parses_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(serde_json::json!({
                "grant_type": "authorization_code",
                "code": "the-code",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_response()))
            .mount(&server)
            .await;

        let provider = AnthropicOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/v1/oauth/token", server.uri()),
            "http://localhost:53692/callback".into(),
        );
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, Some("the-code"))
            .await
            .expect("complete");
        assert_eq!(cred.access_token, "sk-ant-oat01-new");
        assert_eq!(cred.refresh_token, "sk-ant-ort01-rotated");
        assert!(
            cred.expires_at.contains('T'),
            "expires_at should be RFC 3339: {}",
            cred.expires_at
        );
    }

    #[tokio::test]
    async fn refresh_posts_refresh_grant_and_rotates_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .and(body_partial_json(serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": "old-refresh",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_response()))
            .mount(&server)
            .await;

        let provider = AnthropicOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/v1/oauth/token", server.uri()),
            "http://localhost:53692/callback".into(),
        );
        let prior = OAuthCredentialData {
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("user@example.com".into()),
            api_base_url: None,
            project_id: None,
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        assert_eq!(refreshed.access_token, "sk-ant-oat01-new");
        assert_eq!(refreshed.refresh_token, "sk-ant-ort01-rotated");
        // Account carries forward across refresh — the token
        // endpoint doesn't re-emit it.
        assert_eq!(refreshed.account.as_deref(), Some("user@example.com"));
    }

    #[tokio::test]
    async fn refresh_surfaces_http_error_body_in_error_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("{\"error\":\"invalid_grant\"}"),
            )
            .mount(&server)
            .await;

        let provider = AnthropicOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/v1/oauth/token", server.uri()),
            "http://localhost:53692/callback".into(),
        );
        let prior =
            OAuthCredentialData::new("old".into(), "old".into(), "2026-04-20T00:00:00Z".into());
        // `{err:#}` flattens the anyhow context chain so the
        // inner HTTP-status + body strings surface in the output.
        let err = format!("{:#}", provider.refresh(&prior).await.unwrap_err());
        assert!(err.contains("HTTP 400"), "{err}");
        assert!(err.contains("invalid_grant"), "{err}");
    }
}
