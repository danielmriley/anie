# Plan 07 — OAuth auth (Claude Code-style)

**Tier 4 — largest, adds a new auth modality and onboarding flow.**

## Rationale

anie today supports exactly one auth modality: API keys in
`auth.json` (or env vars). pi supports both API keys AND OAuth
with automatic refresh + race-safe locking
(`packages/coding-agent/src/core/auth-storage.ts:369`).

The concrete motivators:

- **Claude Code authentication.** Anthropic's first-party auth
  flow for Claude Code uses OAuth. Users who've already logged in
  via Claude Code want anie to reuse those credentials instead
  of managing a second API key.
- **Consumer login flows in general.** Some providers (Google,
  GitHub Copilot, possibly OpenAI in the future) expose login-
  based access that doesn't issue a long-lived API key.
- **Token refresh.** OAuth access tokens are short-lived. The
  agent needs to refresh transparently without interrupting the
  user — and without letting two concurrent runs both try to
  refresh at once.

## Non-goals

- **Full OAuth 2.0 client credentials.** We need Authorization
  Code + Refresh, and probably Device Authorization (PKCE) for
  CLI flows. Client Credentials grant is for server-to-server,
  not interactive users.
- **UI for every provider's quirks.** Start with Claude Code;
  extend when a second OAuth provider matters.
- **Stored refresh tokens in an OS keyring.** We're not adopting
  a keyring for API keys either; file-mode-0600 storage matches
  pi's approach and is good enough for now.

## Design

### New auth-storage entries

Today `auth.json` is a flat map of `provider_name → api_key`. We
extend the shape to support two credential types per provider:

```json
{
    "openai": {
        "type": "api_key",
        "key": "sk-..."
    },
    "anthropic": {
        "type": "oauth",
        "access_token": "sk-ant-oat01-...",
        "refresh_token": "sk-ant-ort01-...",
        "expires_at": "2026-04-25T14:00:00Z",
        "account": "user@example.com"
    }
}
```

The old format (bare string value) still loads as `api_key` via
a serde untagged-enum default. Forward-compat via serde defaults
and an explicit `type` tag.

### Token-refresh flow

A trait in `anie-auth`:

```rust
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Device-code / authorization-code flow initiation.
    async fn begin_login(&self) -> Result<LoginFlow>;

    /// Exchange the authorization code / device token for an
    /// access + refresh pair.
    async fn complete_login(&self, flow: LoginFlow, code: &str)
        -> Result<OAuthCredential>;

    /// Refresh an expired token. The caller already holds the
    /// per-provider lock (see below).
    async fn refresh(&self, credential: &OAuthCredential)
        -> Result<OAuthCredential>;
}
```

Built-in impls:

- `AnthropicOAuthProvider` — targets Anthropic's Claude Code
  login endpoint.

### Race-safe refresh

pi uses a per-provider mutex with fs-level locking to prevent two
agent runs from refreshing the same token simultaneously. Port
the same idea via `fs2`'s advisory file lock on a per-provider
lock file:

```
~/.anie/auth.lock/<provider>.lock
```

Flow:

1. Agent calls `AuthResolver::resolve(provider_name)`.
2. Resolver reads `auth.json`; finds `type: "oauth"`.
3. If `expires_at > now + 60s`, return the cached access token.
4. Else acquire `<provider>.lock` (blocking, with timeout).
5. Re-read `auth.json` — another process may have already
   refreshed (common with concurrent runs).
6. If still expired, call `OAuthProvider::refresh`, persist the
   new credential, release lock.
7. Return the access token.

### Interactive login flow

Via a new `/login <provider>` slash command that:

1. Opens `OAuthProvider::begin_login` → displays a device code +
   verification URL to the user.
2. Polls the token endpoint until the user completes auth in
   their browser.
3. Stores the credential.
4. Emits a system message confirming login.

Onboarding integration: the provider-preset flow (the one users
see on `/onboard`) grows a "Log in with <provider>" option
alongside "Paste API key" for OAuth-capable providers.

### CLI support

For non-interactive environments (CI, scripts):

```
anie login anthropic
```

Runs the same flow but without the TUI — prints the verification
URL to stdout, polls for completion. Pairs with `anie logout
anthropic` for clearing credentials.

## Files to touch

| File | Change |
|------|--------|
| `crates/anie-auth/src/lib.rs` | Extended `Credential` enum (`ApiKey`, `OAuth`), `OAuthProvider` trait, resolver updates. |
| `crates/anie-auth/src/oauth.rs` | New. Token refresh + file locking. |
| `crates/anie-auth/src/anthropic_oauth.rs` | New. Anthropic-specific endpoints. |
| `crates/anie-auth/Cargo.toml` | Add `fs2` (advisory file locking), `url`, `oauth2` or a simpler handwritten client. |
| `crates/anie-cli/src/commands.rs` | `anie login` / `anie logout` subcommands. |
| `crates/anie-tui/src/overlays/onboarding.rs` | OAuth option alongside API-key option for OAuth-capable providers. |
| `crates/anie-tui/src/commands/` | `/login` / `/logout` slash commands. |
| `crates/anie-auth/src/lib.rs` tests | Credential roundtrip, refresh lock, format migration. |

## Phased PRs

### PR A — extended `Credential` type + format migration

1. Refactor `auth.json` schema to the tagged-enum form.
2. Serde compat: the existing flat-string format loads as
   `Credential::ApiKey(String)`.
3. All resolver paths return `String` (the access token for
   OAuth, the key for API key). No behavior change for existing
   users.
4. Tests:
   - Old `auth.json` loads correctly.
   - New format round-trips.
   - Mixed old/new providers in the same file work.

### PR B — OAuth provider trait + Anthropic impl

1. `OAuthProvider` trait as above.
2. `AnthropicOAuthProvider` — hardcoded endpoints, PKCE device
   flow per Claude Code's published protocol. (Research needed
   at implementation time — check Anthropic's docs.)
3. Unit tests with mocked HTTP endpoints (using `wiremock` or
   `mockito`).

### PR C — refresh-with-lock

1. Per-provider advisory file lock via `fs2::FileExt`.
2. Read-refresh-write dance with lock held.
3. Tests:
   - Token near expiry auto-refreshes.
   - Two concurrent processes don't both refresh (test with
     a synthetic `OAuthProvider` that counts refresh calls).
   - Lock timeout surfaces a typed error.

### PR D — CLI + TUI integration

1. `anie login <provider>` subcommand.
2. `/login` slash command in the TUI.
3. Onboarding preset-flow branch for OAuth-capable providers.
4. Display logged-in state in `/providers` overlay.
5. `anie logout <provider>` clears credentials.
6. End-to-end manual: log in, use the agent, verify tokens
   refresh transparently at expiry boundary.

### PR E — second OAuth provider (optional, if motivated)

Pick whichever OAuth provider a user surfaces demand for first.
Same shape as AnthropicOAuthProvider, just different endpoint
URLs. Ship only when motivated; don't speculatively add.

## Test plan

| # | Test | Where |
|---|------|-------|
| 1 | `auth_file_old_format_loads_as_api_key_credential` | PR A |
| 2 | `auth_file_oauth_format_roundtrips` | PR A |
| 3 | `mixed_format_auth_file_loads_both_entries` | PR A |
| 4 | `oauth_provider_trait_compiles` | PR B |
| 5 | `anthropic_oauth_refresh_updates_expires_at` (mocked) | PR B |
| 6 | `auth_resolver_refreshes_expired_oauth_token` | PR C |
| 7 | `concurrent_refresh_coalesces_via_lock` | PR C |
| 8 | `login_command_persists_credential` (mocked) | PR D |
| 9 | `logout_command_removes_credential` | PR D |

## Risks

- **OAuth endpoint instability.** Providers change their auth
  flows; Anthropic's Claude Code login is relatively stable but
  not a published stable API. Mitigation: isolate endpoints in
  the provider-specific file, put them under a "verified
  against docs on <date>" comment like we do for provider
  SSE formats.
- **Refresh race edge cases.** File locks on macOS and Linux
  behave slightly differently. The `fs2` crate abstracts this;
  test on both platforms before shipping.
- **Keyring revisit.** If a future security audit says "tokens
  on disk is unacceptable," we're already using `mode 0600`
  which matches pi; moving to keyring is a drop-in swap on the
  storage backend without disturbing the flow.
- **Browser-opening UX.** For CLI flows the agent can print a
  URL and let the user open it manually. For interactive TUI
  flows, we'd want to `open` the URL automatically on the
  user's system (via the `opener` crate). First pass: print
  the URL and let the user click; polish later.

## Exit criteria

- [ ] All four required PRs (A-D) merged.
- [ ] `auth.json` migrates cleanly; old configs continue to work.
- [ ] Claude Code OAuth login succeeds end-to-end (manual).
- [ ] Token refresh happens transparently near expiry.
- [ ] Concurrent agents don't both refresh (test + inspection).
- [ ] Logout clears credentials.

## Deferred

- **OS keyring storage.** Matches pi's current state — revisit
  together.
- **Multi-account per provider.** One `account` field per
  provider for now; multi-account needs a session-scoped
  account picker.
- **SSO / SAML flows.** Enterprise shape, out of scope.
