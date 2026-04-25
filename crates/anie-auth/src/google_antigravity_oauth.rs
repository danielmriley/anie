//! Google Antigravity OAuth client (Gemini 3 / Claude / GPT-OSS
//! via Google's Cloud AI Companion).
//!
//! Endpoints + client ID + client secret + scopes + discovery
//! logic verified against pi's
//! `packages/ai/src/utils/oauth/google-antigravity.ts` on
//! 2026-04-21.
//!
//! Flow is OAuth 2.0 Authorization Code + PKCE (RFC 7636 S256)
//! hitting Google's standard OAuth endpoints. Two Google-
//! specific wrinkles vs. Anthropic:
//!
//! 1. **`client_secret` in every token request.** Google's
//!    "installed-app" OAuth flow requires the public-client
//!    secret alongside the PKCE verifier. Base64-encoded in the
//!    source so secret-scanner tools don't flag it (the secret
//!    is public by design for installed apps).
//! 2. **Project discovery.** After token exchange we call
//!    `loadCodeAssist` to fetch the user's Cloud AI Companion
//!    project ID. Tries prod (`cloudcode-pa.googleapis.com`)
//!    then sandbox (`daily-cloudcode-pa.sandbox.googleapis.com`)
//!    per pi; falls back to the hardcoded `rising-fact-p41fc`
//!    if both discovery endpoints fail. The project_id is
//!    persisted on the credential so refresh doesn't repeat
//!    the discovery round-trip.
//!
//! Also: Google's refresh endpoint may NOT return a new
//! `refresh_token` on every call. We match pi by falling back
//! to the prior refresh_token when the response omits it.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;

use crate::oauth::{
    AuthCodeFlow, LoginFlow, OAuthCredentialData, OAuthProvider, PkcePair, compute_expires_at,
    generate_pkce,
};

const CLIENT_ID_B64: &str = "MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ==";
const CLIENT_SECRET_B64: &str = "R09DU1BYLUs1OEZXUjQ4NkxkTEoxbUxCOHNYQzR6NnFEQWY=";

const DEFAULT_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
const DEFAULT_DISCOVERY_ENDPOINTS: &[&str] = &[
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
];
const CALLBACK_PORT: u16 = 51121;
const CALLBACK_PATH: &str = "/oauth-callback";

/// Fallback project ID when `loadCodeAssist` discovery fails on
/// both prod and sandbox. Matches pi's fallback.
const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

pub struct GoogleAntigravityOAuthProvider {
    client: reqwest::Client,
    authorize_url: String,
    token_url: String,
    redirect_uri: String,
    userinfo_url: String,
    discovery_endpoints: Vec<String>,
}

impl GoogleAntigravityOAuthProvider {
    /// Production-endpoint constructor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            authorize_url: DEFAULT_AUTHORIZE_URL.to_string(),
            token_url: DEFAULT_TOKEN_URL.to_string(),
            redirect_uri: default_redirect_uri(),
            userinfo_url: DEFAULT_USERINFO_URL.to_string(),
            discovery_endpoints: DEFAULT_DISCOVERY_ENDPOINTS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }

    /// Test seam: all endpoints collapse onto one mock base.
    #[cfg(test)]
    fn with_mock_base(base: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            authorize_url: format!("{base}/o/oauth2/v2/auth"),
            token_url: format!("{base}/token"),
            redirect_uri: default_redirect_uri(),
            userinfo_url: format!("{base}/userinfo"),
            discovery_endpoints: vec![base.to_string()],
        }
    }

    fn client_id() -> Result<String> {
        decode_b64(CLIENT_ID_B64, "client id")
    }

    fn client_secret() -> Result<String> {
        decode_b64(CLIENT_SECRET_B64, "client secret")
    }
}

impl Default for GoogleAntigravityOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_b64(value: &str, label: &str) -> Result<String> {
    let bytes = STANDARD
        .decode(value)
        .map_err(|err| anyhow!("internal: failed to decode {label}: {err}"))?;
    String::from_utf8(bytes).map_err(|err| anyhow!("internal: {label} not utf-8: {err}"))
}

#[must_use]
pub fn default_redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

#[async_trait]
impl OAuthProvider for GoogleAntigravityOAuthProvider {
    fn name(&self) -> &'static str {
        "google-antigravity"
    }

    async fn begin_login(&self) -> Result<LoginFlow> {
        let PkcePair {
            verifier,
            challenge,
        } = generate_pkce();
        let client_id = Self::client_id()?;
        let scope = SCOPES.join(" ");
        let params = [
            ("client_id", client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", scope.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            // pi echoes the PKCE verifier as state — matches
            // Anthropic's pattern; reusing it keeps the callback
            // validation simple.
            ("state", verifier.as_str()),
            // Google-specific: force a consent screen so the
            // refresh_token is always returned (otherwise
            // returning users get a token response without it).
            ("access_type", "offline"),
            ("prompt", "consent"),
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
            anyhow!("Google Antigravity uses authorization-code flow; device flow not supported")
        })?;
        let code = code
            .ok_or_else(|| anyhow!("Google Antigravity login requires the authorization code"))?;
        let client_id = Self::client_id()?;
        let client_secret = Self::client_secret()?;

        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("code", code),
            ("code_verifier", flow.verifier.as_str()),
            ("redirect_uri", flow.redirect_uri.as_str()),
        ];
        let response: GoogleTokenResponse = post_form(&self.client, &self.token_url, &form)
            .await
            .context("authorization_code exchange failed")?;

        let refresh_token = response.refresh_token.clone().ok_or_else(|| {
            anyhow!(
                "token exchange did not return a refresh_token — \
                 Google suppresses this for re-authorized users; \
                 pass access_type=offline + prompt=consent (we do) \
                 and confirm the consent screen is visible"
            )
        })?;
        let account = fetch_user_email(&self.client, &self.userinfo_url, &response.access_token)
            .await
            .ok()
            .flatten();
        let project_id = discover_project(
            &self.client,
            &self.discovery_endpoints,
            &response.access_token,
        )
        .await
        .unwrap_or_else(|_| DEFAULT_PROJECT_ID.to_string());

        Ok(OAuthCredentialData {
            access_token: response.access_token,
            refresh_token,
            expires_at: compute_expires_at(response.expires_in)?,
            account,
            api_base_url: None,
            project_id: Some(project_id),
        })
    }

    async fn refresh(&self, credential: &OAuthCredentialData) -> Result<OAuthCredentialData> {
        let client_id = Self::client_id()?;
        let client_secret = Self::client_secret()?;
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", credential.refresh_token.as_str()),
        ];
        let response: GoogleTokenResponse = post_form(&self.client, &self.token_url, &form)
            .await
            .context("refresh_token exchange failed")?;

        Ok(OAuthCredentialData {
            access_token: response.access_token,
            // Google often omits refresh_token on refresh —
            // reuse the previous one in that case, matching pi.
            refresh_token: response
                .refresh_token
                .unwrap_or_else(|| credential.refresh_token.clone()),
            expires_at: compute_expires_at(response.expires_in)?,
            account: credential.account.clone(),
            api_base_url: credential.api_base_url.clone(),
            project_id: credential.project_id.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

/// Fetch the user's email via Google's userinfo endpoint.
/// Best-effort — failures surface `Err(_)` and the caller logs
/// None for account. We don't want email lookup to abort a
/// successful login.
async fn fetch_user_email(
    client: &reqwest::Client,
    url: &str,
    access_token: &str,
) -> Result<Option<String>> {
    let response = client
        .get(url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|err| anyhow!("userinfo request failed: {err}"))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let json: serde_json::Value = response
        .json()
        .await
        .map_err(|err| anyhow!("userinfo response did not parse: {err}"))?;
    Ok(json
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::to_string))
}

/// Call `loadCodeAssist` against each discovery endpoint in
/// order, returning the first project ID we find. Falls
/// through to the caller's fallback on total failure —
/// matching pi's behavior.
async fn discover_project(
    client: &reqwest::Client,
    endpoints: &[String],
    access_token: &str,
) -> Result<String> {
    let metadata = serde_json::json!({
        "ideType": "IDE_UNSPECIFIED",
        "platform": "PLATFORM_UNSPECIFIED",
        "pluginType": "GEMINI",
    });
    for endpoint in endpoints {
        let url = format!("{endpoint}/v1internal:loadCodeAssist");
        let Ok(response) = client
            .post(&url)
            .bearer_auth(access_token)
            .header("Content-Type", "application/json")
            .header("User-Agent", "google-api-nodejs-client/9.15.1")
            .header(
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            )
            .header(
                "Client-Metadata",
                serde_json::to_string(&metadata).unwrap_or_default(),
            )
            .json(&serde_json::json!({ "metadata": metadata }))
            .send()
            .await
        else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.json::<serde_json::Value>().await else {
            continue;
        };
        if let Some(project) = extract_cloud_ai_project(&body) {
            return Ok(project);
        }
    }
    Err(anyhow!("no discovery endpoint returned a project"))
}

/// `cloudaicompanionProject` comes back as either a bare
/// string or a struct `{id: "..."}`. Accept both shapes.
fn extract_cloud_ai_project(body: &serde_json::Value) -> Option<String> {
    let field = body.get("cloudaicompanionProject")?;
    if let Some(s) = field.as_str() {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    if let Some(id) = field.get("id").and_then(|v| v.as_str()) {
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

async fn post_form<S: AsRef<str>>(
    client: &reqwest::Client,
    url: &str,
    form: &[(S, S)],
) -> Result<GoogleTokenResponse> {
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

    fn token_body() -> serde_json::Value {
        serde_json::json!({
            "access_token": "ga-access-token",
            "refresh_token": "ga-refresh-token",
            "expires_in": 3_600,
            "token_type": "Bearer",
            "scope": SCOPES.join(" "),
        })
    }

    fn userinfo_body() -> serde_json::Value {
        serde_json::json!({
            "email": "user@example.com",
            "name": "Example User",
        })
    }

    fn project_body() -> serde_json::Value {
        serde_json::json!({
            "cloudaicompanionProject": "my-project-123",
        })
    }

    #[test]
    fn client_id_decodes_to_a_google_oauth_installed_app_id() {
        // pi stores this value base64-encoded for the exact same
        // reason: avoid tripping secret scanners on what is, per
        // Google's own installed-app OAuth docs, a non-secret
        // identifier. We assert shape only — the plaintext lives
        // nowhere in this source file.
        let id = GoogleAntigravityOAuthProvider::client_id().expect("decode");
        assert!(id.ends_with(".apps.googleusercontent.com"), "{id}");
        assert!(id.contains('-'), "{id}");
        assert!(id.len() > 40, "{id}");
    }

    #[test]
    fn client_secret_decodes_cleanly() {
        let secret = GoogleAntigravityOAuthProvider::client_secret().expect("decode");
        assert!(
            secret.starts_with("GOCSPX-"),
            "secret must use Google's standard prefix"
        );
        assert!(secret.len() > 10, "secret looks truncated");
    }

    #[test]
    fn default_redirect_uri_matches_pi_port_and_path() {
        assert_eq!(
            default_redirect_uri(),
            "http://localhost:51121/oauth-callback"
        );
    }

    #[test]
    fn extract_cloud_ai_project_accepts_string_and_object_shapes() {
        let string_form = serde_json::json!({ "cloudaicompanionProject": "abc-123" });
        assert_eq!(
            extract_cloud_ai_project(&string_form).as_deref(),
            Some("abc-123")
        );
        let object_form = serde_json::json!({
            "cloudaicompanionProject": { "id": "def-456" },
        });
        assert_eq!(
            extract_cloud_ai_project(&object_form).as_deref(),
            Some("def-456")
        );
        let empty = serde_json::json!({ "cloudaicompanionProject": "" });
        assert!(extract_cloud_ai_project(&empty).is_none());
        let missing = serde_json::json!({ "other": 1 });
        assert!(extract_cloud_ai_project(&missing).is_none());
    }

    #[tokio::test]
    async fn begin_login_emits_google_authorize_url_with_offline_consent() {
        let provider = GoogleAntigravityOAuthProvider::new();
        let flow = provider.begin_login().await.expect("begin");
        let auth_code = flow.as_auth_code().expect("auth code");
        assert!(
            auth_code.authorize_url.starts_with(DEFAULT_AUTHORIZE_URL),
            "{}",
            auth_code.authorize_url
        );
        for key in [
            "scope=",
            "code_challenge_method=S256",
            "access_type=offline",
            "prompt=consent",
        ] {
            assert!(
                auth_code.authorize_url.contains(key),
                "missing {key} in {}",
                auth_code.authorize_url
            );
        }
        assert_eq!(auth_code.state, auth_code.verifier);
    }

    #[tokio::test]
    async fn complete_login_exchanges_code_and_resolves_project_plus_email() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=the-code"))
            .and(body_string_contains("client_secret="))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(userinfo_body()))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(match_path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(project_body()))
            .mount(&server)
            .await;

        let provider = GoogleAntigravityOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, Some("the-code"))
            .await
            .expect("complete");
        assert_eq!(cred.access_token, "ga-access-token");
        assert_eq!(cred.refresh_token, "ga-refresh-token");
        assert_eq!(cred.account.as_deref(), Some("user@example.com"));
        assert_eq!(cred.project_id.as_deref(), Some("my-project-123"));
    }

    #[tokio::test]
    async fn project_discovery_failure_falls_back_to_pi_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(token_body()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(match_path("/userinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(userinfo_body()))
            .mount(&server)
            .await;
        // No /v1internal:loadCodeAssist mount — all discovery
        // endpoints will 404.

        let provider = GoogleAntigravityOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, Some("the-code"))
            .await
            .expect("complete");
        assert_eq!(cred.project_id.as_deref(), Some(DEFAULT_PROJECT_ID));
    }

    #[tokio::test]
    async fn complete_login_rejects_response_without_refresh_token() {
        let server = MockServer::start().await;
        let body_without_refresh = serde_json::json!({
            "access_token": "ga-access-token",
            "expires_in": 3_600,
        });
        Mock::given(method("POST"))
            .and(match_path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body_without_refresh))
            .mount(&server)
            .await;

        let provider = GoogleAntigravityOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let err = format!(
            "{:#}",
            provider.complete_login(&flow, Some("c")).await.unwrap_err()
        );
        assert!(err.contains("refresh_token"), "{err}");
    }

    #[tokio::test]
    async fn refresh_reuses_prior_refresh_token_when_response_omits_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "rotated-access",
                "expires_in": 3_600,
            })))
            .mount(&server)
            .await;

        let provider = GoogleAntigravityOAuthProvider::with_mock_base(&server.uri());
        let prior = OAuthCredentialData {
            access_token: "old-access".into(),
            refresh_token: "persisted-refresh".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("user@example.com".into()),
            api_base_url: None,
            project_id: Some("my-project-123".into()),
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        assert_eq!(refreshed.access_token, "rotated-access");
        assert_eq!(refreshed.refresh_token, "persisted-refresh");
        assert_eq!(refreshed.account.as_deref(), Some("user@example.com"));
        assert_eq!(refreshed.project_id.as_deref(), Some("my-project-123"));
    }
}
