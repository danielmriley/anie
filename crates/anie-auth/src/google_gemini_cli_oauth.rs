//! Google Gemini CLI OAuth client.
//!
//! Endpoints + client ID + client secret + scopes + discovery
//! logic verified against pi's
//! `packages/ai/src/utils/oauth/google-gemini-cli.ts` on
//! 2026-04-21.
//!
//! Flow is OAuth 2.0 Authorization Code + PKCE, same as
//! Antigravity and with the same Google-specific wrinkles
//! (`client_secret`, refresh-token omission on rotation).
//! What's different is **project discovery**:
//!
//! 1. Call `loadCodeAssist` with (optional) `GOOGLE_CLOUD_PROJECT`
//!    env var. If the user already has a tier + project, use it.
//! 2. If the user has a tier but no project, require the env
//!    var (paid tiers without a project are unsupported without
//!    explicit user config).
//! 3. If no tier, call `onboardUser` which returns a Google
//!    Long-Running Operation. Poll `/v1internal/<op_name>`
//!    until `done=true`, then read `response.cloudaicompanionProject.id`.
//! 4. For VPC-SC affected users (a subset of Workspace accounts),
//!    `loadCodeAssist` returns a `SECURITY_POLICY_VIOLATED` error
//!    which pi paves around by defaulting to the standard tier +
//!    requiring the env var. We match.
//!
//! No hardcoded fallback project ID (unlike Antigravity) — if
//! discovery fails we return an error rather than stuffing a
//! placeholder into the credential.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;

use crate::oauth::{
    AuthCodeFlow, LoginFlow, OAuthCredentialData, OAuthProvider, PkcePair, compute_expires_at,
    generate_pkce,
};

const CLIENT_ID_B64: &str =
    "NjgxMjU1ODA5Mzk1LW9vOGZ0Mm9wcmRybnA5ZTNhcWY2YXYzaG1kaWIxMzVqLmFwcHMuZ29vZ2xldXNlcmNvbnRlbnQuY29t";
const CLIENT_SECRET_B64: &str = "R09DU1BYLTR1SGdNUG0tMW83U2stZ2VWNkN1NWNsWEZzeGw=";

const DEFAULT_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DEFAULT_CODE_ASSIST_BASE: &str = "https://cloudcode-pa.googleapis.com";
const DEFAULT_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v1/userinfo?alt=json";
const CALLBACK_PORT: u16 = 8085;
const CALLBACK_PATH: &str = "/oauth2callback";

const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
];

/// Tier IDs per pi's `google-gemini-cli.ts`. `free-tier` is
/// the default for new personal accounts.
const TIER_FREE: &str = "free-tier";
const TIER_LEGACY: &str = "legacy-tier";

/// Maximum number of LRO poll attempts. pi polls indefinitely;
/// we bound ours so a broken provisioning doesn't hang the
/// whole login flow.
const MAX_LRO_POLL_ATTEMPTS: u32 = 60;
const LRO_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct GeminiCliOAuthProvider {
    client: reqwest::Client,
    authorize_url: String,
    token_url: String,
    redirect_uri: String,
    userinfo_url: String,
    code_assist_base: String,
    /// Override `GOOGLE_CLOUD_PROJECT[_ID]` for tests — prod
    /// code leaves this `None` and reads the real env vars.
    env_project_override: Option<Option<String>>,
    /// Test seam: collapse LRO poll interval so test runs
    /// don't sleep for seconds.
    poll_interval: Duration,
}

impl GeminiCliOAuthProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            authorize_url: DEFAULT_AUTHORIZE_URL.to_string(),
            token_url: DEFAULT_TOKEN_URL.to_string(),
            redirect_uri: default_redirect_uri(),
            userinfo_url: DEFAULT_USERINFO_URL.to_string(),
            code_assist_base: DEFAULT_CODE_ASSIST_BASE.to_string(),
            env_project_override: None,
            poll_interval: LRO_POLL_INTERVAL,
        }
    }

    /// Test seam.
    #[cfg(test)]
    fn with_mock_base(base: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            authorize_url: format!("{base}/o/oauth2/v2/auth"),
            token_url: format!("{base}/token"),
            redirect_uri: default_redirect_uri(),
            userinfo_url: format!("{base}/userinfo"),
            code_assist_base: base.to_string(),
            env_project_override: Some(None),
            poll_interval: Duration::from_millis(10),
        }
    }

    #[cfg(test)]
    fn with_env_project(mut self, project: Option<&str>) -> Self {
        self.env_project_override = Some(project.map(str::to_string));
        self
    }

    fn client_id() -> Result<String> {
        decode_b64(CLIENT_ID_B64, "client id")
    }

    fn client_secret() -> Result<String> {
        decode_b64(CLIENT_SECRET_B64, "client secret")
    }

    /// Read the env-provided project ID (test override takes
    /// precedence) or fall back to `GOOGLE_CLOUD_PROJECT`
    /// / `GOOGLE_CLOUD_PROJECT_ID`.
    fn env_project(&self) -> Option<String> {
        if let Some(override_val) = &self.env_project_override {
            return override_val.clone();
        }
        std::env::var("GOOGLE_CLOUD_PROJECT")
            .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT_ID"))
            .ok()
    }
}

impl Default for GeminiCliOAuthProvider {
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
impl OAuthProvider for GeminiCliOAuthProvider {
    fn name(&self) -> &'static str {
        "google-gemini-cli"
    }

    async fn begin_login(&self) -> Result<LoginFlow> {
        let PkcePair { verifier, challenge } = generate_pkce();
        let client_id = Self::client_id()?;
        let scope = SCOPES.join(" ");
        let params = [
            ("client_id", client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("scope", scope.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", verifier.as_str()),
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
            anyhow!("Google Gemini CLI uses authorization-code flow; device flow not supported")
        })?;
        let code = code
            .ok_or_else(|| anyhow!("Google Gemini CLI login requires the authorization code"))?;
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
        let response: GoogleTokenResponse =
            post_form(&self.client, &self.token_url, &form)
                .await
                .context("authorization_code exchange failed")?;

        let refresh_token = response.refresh_token.clone().ok_or_else(|| {
            anyhow!(
                "token exchange did not return a refresh_token — \
                 pass access_type=offline + prompt=consent (we do) \
                 and confirm the consent screen is visible"
            )
        })?;
        let account = fetch_user_email(&self.client, &self.userinfo_url, &response.access_token)
            .await
            .ok()
            .flatten();
        let project_id = self
            .discover_project(&response.access_token)
            .await
            .context("Cloud Code Assist project discovery failed")?;

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
        let response: GoogleTokenResponse =
            post_form(&self.client, &self.token_url, &form)
                .await
                .context("refresh_token exchange failed")?;

        Ok(OAuthCredentialData {
            access_token: response.access_token,
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

impl GeminiCliOAuthProvider {
    /// Full discovery sequence: loadCodeAssist → onboardUser
    /// LRO polling. Returns the user's Cloud AI Companion
    /// project ID. pi does not define a fallback; neither do
    /// we — discovery failures error rather than stash a
    /// placeholder.
    async fn discover_project(&self, access_token: &str) -> Result<String> {
        let env_project = self.env_project();
        let metadata = serde_json::json!({
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
            "duetProject": env_project,
        });

        let load_url = format!("{}/v1internal:loadCodeAssist", self.code_assist_base);
        let load_body = serde_json::json!({
            "cloudaicompanionProject": env_project,
            "metadata": metadata,
        });
        let load_response = self
            .client
            .post(&load_url)
            .bearer_auth(access_token)
            .json(&load_body)
            .send()
            .await
            .map_err(|err| anyhow!("loadCodeAssist request failed: {err}"))?;
        let load_status = load_response.status();
        let load_text = load_response
            .text()
            .await
            .map_err(|err| anyhow!("failed to read loadCodeAssist body: {err}"))?;

        let payload: LoadCodeAssistPayload = if load_status.is_success() {
            serde_json::from_str(&load_text).map_err(|err| {
                anyhow!("loadCodeAssist response did not parse ({err}); body was: {load_text}")
            })?
        } else if is_vpc_sc_violation(&load_text) {
            // Fallback: pretend the user has a 'standard' tier
            // per pi so we drop into the "need GOOGLE_CLOUD_PROJECT"
            // error path with the right message.
            LoadCodeAssistPayload {
                current_tier: Some(Tier {
                    id: Some("standard-tier".into()),
                }),
                cloudai_companion_project: None,
                allowed_tiers: None,
            }
        } else {
            return Err(anyhow!(
                "loadCodeAssist returned HTTP {load_status} (body: {load_text})"
            ));
        };

        // Happy path: user has a current tier AND a project.
        if payload.current_tier.is_some() {
            if let Some(project) = payload.cloudai_companion_project.and_then(|p| p.into_id()) {
                return Ok(project);
            }
            // Tier without project → must have env_project set.
            if let Some(project) = env_project {
                return Ok(project);
            }
            return Err(anyhow!(
                "account requires setting GOOGLE_CLOUD_PROJECT or \
                 GOOGLE_CLOUD_PROJECT_ID. See \
                 https://goo.gle/gemini-cli-auth-docs#workspace-gca"
            ));
        }

        // Onboarding path: new personal account, free tier,
        // Google will provision a project for us.
        let tier_id = payload
            .allowed_tiers
            .as_deref()
            .and_then(default_tier)
            .unwrap_or_else(|| TIER_LEGACY.to_string());
        let effective_tier = if tier_id == "standard-tier" || tier_id == TIER_LEGACY {
            // Workspace-like accounts require user-supplied project.
            if env_project.is_none() {
                return Err(anyhow!(
                    "account's default tier is '{tier_id}' which requires \
                     GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID. See \
                     https://goo.gle/gemini-cli-auth-docs#workspace-gca"
                ));
            }
            tier_id
        } else {
            // Free tier: Google provisions a project for us.
            TIER_FREE.to_string()
        };

        let mut onboard_body = serde_json::json!({
            "tierId": effective_tier,
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            },
        });
        if effective_tier != TIER_FREE {
            if let Some(project) = &env_project {
                onboard_body["cloudaicompanionProject"] =
                    serde_json::Value::String(project.clone());
                onboard_body["metadata"]["duetProject"] =
                    serde_json::Value::String(project.clone());
            }
        }

        let onboard_url = format!("{}/v1internal:onboardUser", self.code_assist_base);
        let onboard_response = self
            .client
            .post(&onboard_url)
            .bearer_auth(access_token)
            .json(&onboard_body)
            .send()
            .await
            .map_err(|err| anyhow!("onboardUser request failed: {err}"))?;
        let onboard_status = onboard_response.status();
        let onboard_text = onboard_response
            .text()
            .await
            .map_err(|err| anyhow!("failed to read onboardUser body: {err}"))?;
        if !onboard_status.is_success() {
            return Err(anyhow!(
                "onboardUser returned HTTP {onboard_status} (body: {onboard_text})"
            ));
        }
        let mut lro: LongRunningOperation = serde_json::from_str(&onboard_text).map_err(|err| {
            anyhow!("onboardUser response did not parse ({err}); body was: {onboard_text}")
        })?;

        if !lro.done.unwrap_or(false) {
            let operation_name = lro.name.clone().ok_or_else(|| {
                anyhow!("onboardUser returned an unfinished LRO without a name")
            })?;
            lro = self.poll_lro(access_token, &operation_name).await?;
        }

        lro.response
            .and_then(|resp| resp.cloudai_companion_project?.id)
            .ok_or_else(|| anyhow!("onboardUser completed without a project id in the response"))
    }

    async fn poll_lro(
        &self,
        access_token: &str,
        operation_name: &str,
    ) -> Result<LongRunningOperation> {
        let url = format!("{}/v1internal/{}", self.code_assist_base, operation_name);
        for _ in 0..MAX_LRO_POLL_ATTEMPTS {
            tokio::time::sleep(self.poll_interval).await;
            let response = self
                .client
                .get(&url)
                .bearer_auth(access_token)
                .send()
                .await
                .map_err(|err| anyhow!("LRO poll request failed: {err}"))?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|err| anyhow!("failed to read LRO poll body: {err}"))?;
            if !status.is_success() {
                return Err(anyhow!("LRO poll returned HTTP {status} (body: {text})"));
            }
            let lro: LongRunningOperation = serde_json::from_str(&text).map_err(|err| {
                anyhow!("LRO poll response did not parse ({err}); body was: {text}")
            })?;
            if lro.done.unwrap_or(false) {
                return Ok(lro);
            }
        }
        Err(anyhow!(
            "onboardUser LRO did not complete after {MAX_LRO_POLL_ATTEMPTS} attempts"
        ))
    }
}

#[derive(Debug, Deserialize)]
struct GoogleTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct LoadCodeAssistPayload {
    #[serde(rename = "currentTier", default)]
    current_tier: Option<Tier>,
    #[serde(rename = "cloudaicompanionProject", default)]
    cloudai_companion_project: Option<CloudAiProjectField>,
    #[serde(rename = "allowedTiers", default)]
    allowed_tiers: Option<Vec<AllowedTier>>,
}

#[derive(Debug, Deserialize)]
struct Tier {
    // `id` isn't read by the discovery branch (we only care
    // whether `currentTier` is present), but it's part of the
    // on-the-wire shape — dropping it would break if Google
    // ever adds a sibling we DO care about. #[allow] documents
    // the intentional read-none state.
    #[allow(dead_code)]
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AllowedTier {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "isDefault", default)]
    is_default: bool,
}

/// Matches pi's dual-shape field where the project comes back
/// as either `"xyz"` or `{"id": "xyz"}`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CloudAiProjectField {
    String(String),
    Object { id: Option<String> },
}

impl CloudAiProjectField {
    /// Consumes the field to return the project ID if present.
    /// Takes `self` by value because the String variant wants
    /// to move its owned content out — `as_*` naming is a
    /// clippy hint we override here with `into_id`.
    fn into_id(self) -> Option<String> {
        match self {
            Self::String(s) if !s.is_empty() => Some(s),
            Self::Object { id: Some(id) } if !id.is_empty() => Some(id),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct LongRunningOperation {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    done: Option<bool>,
    #[serde(default)]
    response: Option<LroResponse>,
}

#[derive(Debug, Deserialize)]
struct LroResponse {
    #[serde(rename = "cloudaicompanionProject", default)]
    cloudai_companion_project: Option<CloudAiProjectObject>,
}

#[derive(Debug, Deserialize)]
struct CloudAiProjectObject {
    #[serde(default)]
    id: Option<String>,
}

fn default_tier(tiers: &[AllowedTier]) -> Option<String> {
    tiers
        .iter()
        .find(|t| t.is_default)
        .and_then(|t| t.id.clone())
}

fn is_vpc_sc_violation(body: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(details) = value
        .get("error")
        .and_then(|e| e.get("details"))
        .and_then(|d| d.as_array())
    else {
        return false;
    };
    details
        .iter()
        .any(|d| d.get("reason").and_then(|r| r.as_str()) == Some("SECURITY_POLICY_VIOLATED"))
}

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
    Ok(json.get("email").and_then(|v| v.as_str()).map(str::to_string))
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
        matchers::{method, path as match_path, path_regex},
    };

    fn token_body() -> serde_json::Value {
        serde_json::json!({
            "access_token": "gemini-access-token",
            "refresh_token": "gemini-refresh-token",
            "expires_in": 3_600,
            "token_type": "Bearer",
        })
    }

    fn userinfo_body() -> serde_json::Value {
        serde_json::json!({
            "email": "user@example.com",
        })
    }

    #[test]
    fn client_id_decodes_to_gemini_cli_oauth_id() {
        let id = GeminiCliOAuthProvider::client_id().expect("decode");
        assert_eq!(
            id,
            "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com"
        );
    }

    #[test]
    fn default_redirect_uri_matches_pi_port_and_path() {
        assert_eq!(
            default_redirect_uri(),
            "http://localhost:8085/oauth2callback"
        );
    }

    #[test]
    fn default_tier_prefers_is_default() {
        let tiers = vec![
            AllowedTier {
                id: Some("a".into()),
                is_default: false,
            },
            AllowedTier {
                id: Some("b".into()),
                is_default: true,
            },
        ];
        assert_eq!(default_tier(&tiers).as_deref(), Some("b"));
    }

    #[test]
    fn default_tier_returns_none_when_no_default() {
        let tiers = vec![AllowedTier {
            id: Some("a".into()),
            is_default: false,
        }];
        assert_eq!(default_tier(&tiers), None);
    }

    #[test]
    fn is_vpc_sc_violation_matches_pi_rpc_detail_shape() {
        let body = serde_json::json!({
            "error": {
                "details": [{"reason": "SECURITY_POLICY_VIOLATED"}]
            }
        })
        .to_string();
        assert!(is_vpc_sc_violation(&body));
    }

    #[test]
    fn is_vpc_sc_violation_ignores_unrelated_errors() {
        let body = serde_json::json!({
            "error": {"details": [{"reason": "UNAUTHENTICATED"}]}
        })
        .to_string();
        assert!(!is_vpc_sc_violation(&body));
    }

    #[test]
    fn cloud_ai_project_field_accepts_both_shapes() {
        let string: CloudAiProjectField =
            serde_json::from_value(serde_json::json!("abc")).expect("parse");
        assert_eq!(string.into_id().as_deref(), Some("abc"));
        let object: CloudAiProjectField =
            serde_json::from_value(serde_json::json!({ "id": "def" })).expect("parse");
        assert_eq!(object.into_id().as_deref(), Some("def"));
    }

    #[tokio::test]
    async fn discover_project_returns_existing_project_for_tiered_user() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {"id": TIER_FREE},
                "cloudaicompanionProject": "my-existing-project",
            })))
            .mount(&server)
            .await;

        let provider = GeminiCliOAuthProvider::with_mock_base(&server.uri());
        let project = provider
            .discover_project("access-token")
            .await
            .expect("discover");
        assert_eq!(project, "my-existing-project");
    }

    #[tokio::test]
    async fn discover_project_onboards_free_tier_and_polls_lro_to_completion() {
        let server = MockServer::start().await;

        // loadCodeAssist: no tier (new user).
        Mock::given(method("POST"))
            .and(match_path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "allowedTiers": [
                    {"id": TIER_FREE, "isDefault": true},
                ],
            })))
            .mount(&server)
            .await;

        // onboardUser: returns an unfinished LRO.
        Mock::given(method("POST"))
            .and(match_path("/v1internal:onboardUser"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "operations/provision-abc",
                "done": false,
            })))
            .mount(&server)
            .await;

        // First poll: still not done.
        Mock::given(method("GET"))
            .and(path_regex(r"^/v1internal/operations/provision-abc$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "operations/provision-abc",
                "done": false,
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // Subsequent polls: done with project id.
        Mock::given(method("GET"))
            .and(path_regex(r"^/v1internal/operations/provision-abc$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "operations/provision-abc",
                "done": true,
                "response": {
                    "cloudaicompanionProject": {"id": "onboarded-project-xyz"}
                }
            })))
            .mount(&server)
            .await;

        let provider = GeminiCliOAuthProvider::with_mock_base(&server.uri());
        let project = provider
            .discover_project("access-token")
            .await
            .expect("discover");
        assert_eq!(project, "onboarded-project-xyz");
    }

    #[tokio::test]
    async fn discover_project_errors_on_tiered_user_without_project_and_without_env() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {"id": "standard-tier"},
            })))
            .mount(&server)
            .await;

        let provider =
            GeminiCliOAuthProvider::with_mock_base(&server.uri()).with_env_project(None);
        let err = format!(
            "{:#}",
            provider.discover_project("access-token").await.unwrap_err()
        );
        assert!(err.contains("GOOGLE_CLOUD_PROJECT"), "{err}");
    }

    #[tokio::test]
    async fn discover_project_uses_env_project_when_tier_has_no_project() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {"id": "standard-tier"},
            })))
            .mount(&server)
            .await;

        let provider = GeminiCliOAuthProvider::with_mock_base(&server.uri())
            .with_env_project(Some("user-provided-proj"));
        let project = provider
            .discover_project("access-token")
            .await
            .expect("discover");
        assert_eq!(project, "user-provided-proj");
    }

    #[tokio::test]
    async fn complete_login_end_to_end_populates_email_and_project() {
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
        Mock::given(method("POST"))
            .and(match_path("/v1internal:loadCodeAssist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "currentTier": {"id": TIER_FREE},
                "cloudaicompanionProject": {"id": "existing-proj"},
            })))
            .mount(&server)
            .await;

        let provider = GeminiCliOAuthProvider::with_mock_base(&server.uri());
        let flow = provider.begin_login().await.expect("begin");
        let cred = provider
            .complete_login(&flow, Some("the-code"))
            .await
            .expect("complete");
        assert_eq!(cred.access_token, "gemini-access-token");
        assert_eq!(cred.refresh_token, "gemini-refresh-token");
        assert_eq!(cred.account.as_deref(), Some("user@example.com"));
        assert_eq!(cred.project_id.as_deref(), Some("existing-proj"));
    }

    #[tokio::test]
    async fn refresh_preserves_project_id_across_rotation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(match_path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "rotated",
                "expires_in": 3_600,
            })))
            .mount(&server)
            .await;

        let provider = GeminiCliOAuthProvider::with_mock_base(&server.uri());
        let prior = OAuthCredentialData {
            access_token: "old".into(),
            refresh_token: "persisted-refresh".into(),
            expires_at: "2026-04-20T00:00:00Z".into(),
            account: Some("user@example.com".into()),
            api_base_url: None,
            project_id: Some("my-proj".into()),
        };
        let refreshed = provider.refresh(&prior).await.expect("refresh");
        assert_eq!(refreshed.project_id.as_deref(), Some("my-proj"));
    }
}
