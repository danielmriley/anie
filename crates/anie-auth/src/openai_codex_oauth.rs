//! OpenAI Codex (ChatGPT login) OAuth client.
//!
//! Endpoints + client ID + scopes verified against pi's
//! `packages/ai/src/utils/oauth/openai-codex.ts` on 2026-04-21.
//!
//! Flow is OAuth 2.0 Authorization Code + PKCE (RFC 7636 S256),
//! hitting `https://auth.openai.com`. Three wrinkles vs. the
//! Anthropic client:
//!
//! 1. **Form-encoded token body.** OpenAI's token endpoint
//!    expects `application/x-www-form-urlencoded`, not JSON.
//!    We build a `form` body rather than `json`.
//! 2. **Random state.** Unlike Anthropic (which echoes the
//!    PKCE verifier as state), Codex uses an independent 16-byte
//!    random hex value — matches RFC 6749 guidance more closely.
//! 3. **JWT account extraction.** The access_token is a JWT
//!    carrying a custom claim at `https://api.openai.com/auth`
//!    with the `chatgpt_account_id` inside. We decode the JWT's
//!    middle segment (base64url, no padding — per RFC 7519) and
//!    surface the account id as `OAuthCredentialData.account`.
//!    When the claim is absent we leave it `None` rather than
//!    failing the login; the credential still works.
//!
//! Additional authorize-URL params pi passes verbatim:
//! `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`,
//! and `originator` (set to `anie` here; pi sends `pi`).

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::Deserialize;

use crate::oauth::{
    AuthCodeFlow, LoginFlow, OAuthCredentialData, OAuthProvider, PkcePair, compute_expires_at,
    generate_pkce,
};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const DEFAULT_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPES: &str = "openid profile email offline_access";
const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";

pub struct OpenAICodexOAuthProvider {
    client: reqwest::Client,
    authorize_url: String,
    token_url: String,
    redirect_uri: String,
}

impl OpenAICodexOAuthProvider {
    /// Production-endpoint constructor.
    #[must_use]
    pub fn new() -> Self {
        Self::with_endpoints(
            DEFAULT_AUTHORIZE_URL.to_string(),
            DEFAULT_TOKEN_URL.to_string(),
            default_redirect_uri(),
        )
    }

    /// Test seam: point authorize + token URLs at a mock server.
    #[must_use]
    pub fn with_endpoints(authorize_url: String, token_url: String, redirect_uri: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            authorize_url,
            token_url,
            redirect_uri,
        }
    }
}

impl Default for OpenAICodexOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn default_redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[async_trait]
impl OAuthProvider for OpenAICodexOAuthProvider {
    fn name(&self) -> &'static str {
        "openai-codex"
    }

    async fn begin_login(&self) -> Result<LoginFlow> {
        let PkcePair { verifier, challenge } = generate_pkce();
        let state = generate_state();

        let params = [
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", SCOPES),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            // pi-aligned extras Codex's simplified flow expects.
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            // Identifies us to OpenAI's audit log. pi sends "pi";
            // we send "anie" so the two origins don't conflate.
            ("originator", "anie"),
        ];
        let query = serde_urlencoded::to_string(params)
            .map_err(|err| anyhow!("failed to encode authorize params: {err}"))?;
        let authorize_url = format!("{}?{query}", self.authorize_url);

        Ok(LoginFlow::AuthorizationCode(AuthCodeFlow {
            authorize_url,
            verifier,
            state,
            redirect_uri: self.redirect_uri.clone(),
        }))
    }

    async fn complete_login(
        &self,
        flow: &LoginFlow,
        code: Option<&str>,
    ) -> Result<OAuthCredentialData> {
        let flow = flow.as_auth_code().ok_or_else(|| {
            anyhow!("OpenAI Codex uses authorization-code flow; device flow not supported")
        })?;
        let code = code.ok_or_else(|| {
            anyhow!("OpenAI Codex login requires the authorization code")
        })?;

        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", flow.verifier.as_str()),
            ("redirect_uri", flow.redirect_uri.as_str()),
        ];
        let response: CodexTokenResponse =
            post_form(&self.client, &self.token_url, &form)
                .await
                .context("authorization_code exchange failed")?;

        Ok(build_credential(response, None)?)
    }

    async fn refresh(&self, credential: &OAuthCredentialData) -> Result<OAuthCredentialData> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", credential.refresh_token.as_str()),
            ("client_id", CLIENT_ID),
        ];
        let response: CodexTokenResponse =
            post_form(&self.client, &self.token_url, &form)
                .await
                .context("refresh_token exchange failed")?;

        // Preserve the account label across refresh — the token
        // endpoint returns a fresh JWT that also carries the
        // claim, but we don't want to redo JWT parsing on every
        // refresh. If the stored account is None, try to extract
        // from the new access_token; otherwise carry forward.
        let carry_over = credential.account.clone();
        Ok(build_credential(response, carry_over)?)
    }
}

#[derive(Debug, Deserialize)]
struct CodexTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

fn build_credential(
    response: CodexTokenResponse,
    carry_over_account: Option<String>,
) -> Result<OAuthCredentialData> {
    // JWT claim extraction is best-effort — missing claims
    // degrade to None without failing the login.
    let account = carry_over_account.or_else(|| extract_account_id(&response.access_token));
    Ok(OAuthCredentialData {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: compute_expires_at(response.expires_in)?,
        account,
        api_base_url: None,
        project_id: None,
    })
}

/// Decode the JWT payload (middle segment) and extract
/// `chatgpt_account_id` from the pi-documented custom claim at
/// `https://api.openai.com/auth`. Returns `None` on any parsing
/// failure — we never want a JWT parse error to break login.
fn extract_account_id(access_token: &str) -> Option<String> {
    let segments: Vec<&str> = access_token.split('.').collect();
    if segments.len() != 3 {
        return None;
    }
    let payload_bytes = URL_SAFE_NO_PAD.decode(segments[1]).ok()?;
    let payload: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    payload
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

async fn post_form<S: AsRef<str>>(
    client: &reqwest::Client,
    url: &str,
    form: &[(S, S)],
) -> Result<CodexTokenResponse> {
    let body = serde_urlencoded::to_string(
        form.iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect::<Vec<_>>(),
    )
    .map_err(|err| anyhow!("failed to url-encode token form: {err}"))?;
    let response = client
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
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
    serde_json::from_str(&text)
        .map_err(|err| anyhow!("token response did not parse as JSON ({err}); body was: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path as match_path},
    };

    /// Build a valid-looking JWT with the pi-documented claim
    /// so `extract_account_id` has something to chew on. Header
    /// + signature are placeholder; only the payload is parsed.
    fn jwt_with_account(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload_obj = serde_json::json!({
            JWT_CLAIM_PATH: {
                "chatgpt_account_id": account_id,
            },
        });
        let payload = URL_SAFE_NO_PAD.encode(payload_obj.to_string().as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{payload}.{signature}")
    }

    fn sample_response(account_id: &str) -> serde_json::Value {
        serde_json::json!({
            "access_token": jwt_with_account(account_id),
            "refresh_token": "codex-refresh-token",
            "expires_in": 3_600,
        })
    }

    #[test]
    fn default_redirect_uri_matches_registered_codex_route() {
        assert_eq!(default_redirect_uri(), "http://localhost:1455/auth/callback");
    }

    #[test]
    fn generate_state_produces_32_hex_chars() {
        let s = generate_state();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_state_is_unique_per_call() {
        assert_ne!(generate_state(), generate_state());
    }

    #[test]
    fn extract_account_id_pulls_chatgpt_account_from_jwt_claim() {
        let jwt = jwt_with_account("acct_abc123");
        assert_eq!(extract_account_id(&jwt).as_deref(), Some("acct_abc123"));
    }

    #[test]
    fn extract_account_id_returns_none_for_malformed_jwt() {
        assert!(extract_account_id("not-a-jwt").is_none());
        assert!(extract_account_id("only.two").is_none());
        assert!(extract_account_id("one.two.three").is_none()); // not base64
    }

    #[tokio::test]
    async fn begin_login_produces_authorize_url_with_codex_extras() {
        let provider = OpenAICodexOAuthProvider::new();
        let flow = provider.begin_login().await.expect("begin");
        let auth_code = flow.as_auth_code().expect("auth code");
        assert!(auth_code.authorize_url.starts_with(DEFAULT_AUTHORIZE_URL));
        for key in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "scope=",
            "code_challenge=",
            "code_challenge_method=S256",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=anie",
        ] {
            assert!(
                auth_code.authorize_url.contains(key),
                "missing {key} in {}",
                auth_code.authorize_url
            );
        }
        // State is independent random (hex), not the verifier.
        assert_ne!(auth_code.state, auth_code.verifier);
        assert_eq!(auth_code.state.len(), 32);
    }

    #[tokio::test]
    async fn complete_login_form_encodes_body_and_extracts_account_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=the-code"))
            .and(body_string_contains("code_verifier="))
            .respond_with(ResponseTemplate::new(200).set_body_json(sample_response("acct_42")))
            .mount(&server)
            .await;

        let provider = OpenAICodexOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/oauth/token", server.uri()),
            default_redirect_uri(),
        );
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, Some("the-code"))
            .await
            .expect("complete");
        assert_eq!(cred.account.as_deref(), Some("acct_42"));
        assert_eq!(cred.refresh_token, "codex-refresh-token");
    }

    #[tokio::test]
    async fn refresh_preserves_account_across_rotation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(sample_response("acct_from_new_jwt")),
            )
            .mount(&server)
            .await;

        let provider = OpenAICodexOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/oauth/token", server.uri()),
            default_redirect_uri(),
        );
        let prior = OAuthCredentialData {
            access_token: "old".into(),
            refresh_token: "old-refresh".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("acct_preserved".into()),
            api_base_url: None,
            project_id: None,
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        // The carry-over wins: we don't re-extract from the new
        // JWT unless the prior account was None.
        assert_eq!(refreshed.account.as_deref(), Some("acct_preserved"));
    }

    #[tokio::test]
    async fn refresh_extracts_account_from_new_jwt_when_prior_was_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(sample_response("acct_from_new_jwt")),
            )
            .mount(&server)
            .await;

        let provider = OpenAICodexOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/oauth/token", server.uri()),
            default_redirect_uri(),
        );
        let prior = OAuthCredentialData::new(
            "old".into(),
            "old".into(),
            "2026-04-20T00:00:00Z".into(),
        );
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        assert_eq!(refreshed.account.as_deref(), Some("acct_from_new_jwt"));
    }

    #[tokio::test]
    async fn complete_login_surfaces_http_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/oauth/token"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string("{\"error\":\"invalid_grant\"}"),
            )
            .mount(&server)
            .await;

        let provider = OpenAICodexOAuthProvider::with_endpoints(
            "https://ignored".into(),
            format!("{}/oauth/token", server.uri()),
            default_redirect_uri(),
        );
        let flow = provider.begin_login().await.expect("begin");
        let err = format!(
            "{:#}",
            provider.complete_login(&flow, Some("bad")).await.unwrap_err()
        );
        assert!(err.contains("HTTP 400"), "{err}");
        assert!(err.contains("invalid_grant"), "{err}");
    }
}
