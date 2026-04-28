# 05 — OAuth provider deduplication

**VERY HIGH RISK** — only act when next OAuth provider
addition forces the abstraction. Until then, the duplication
is the cost of provider independence.

## The duplication

Five OAuth providers in `anie-auth/src/`:

| Provider | LOC |
|----------|----:|
| `anthropic_oauth.rs` | 387 |
| `openai_codex_oauth.rs` | 451 |
| `github_copilot_oauth.rs` | 635 |
| `google_antigravity_oauth.rs` | 607 |
| `google_gemini_cli_oauth.rs` | 887 |
| **Total** | **2,967** |

Common boilerplate (~70% per file):
- PKCE challenge generation
- Authorization URL construction
- Local callback server (handled partially in shared
  `callback.rs`)
- Token exchange (POST + JSON parse)
- Refresh token rotation
- Store/retrieve via `CredentialStore`

Per-provider quirks (the part we can't dedup):
- Endpoints (token URL, auth URL)
- Request/response body shapes (`access_token` vs `token`,
  flat vs nested user info)
- Token expiry semantics (some have `expires_in`, some
  send absolute timestamps)
- Custom error 401/403 patterns
- Endpoint discovery (Antigravity/Copilot have project /
  workspace discovery flows)

## Why very high risk

Auth code is security-critical. A bug in the shared
abstraction:
- Could leak credentials cross-provider
- Could break the refresh loop silently (user re-prompted
  every session)
- Could accept malformed tokens
- Is hard to test exhaustively without real OAuth round-trips

Each provider has its own test coverage today. Consolidation
adds failure modes that no single-provider test exercises.

## Proposed shape (sketch only)

```rust
trait AuthCodeFlow {
    const AUTHORIZE_URL: &str;
    const TOKEN_URL: &str;
    type AuthResponse: DeserializeOwned;
    type RefreshResponse: DeserializeOwned;

    fn map_auth_response(r: Self::AuthResponse) -> AuthCredential;
    fn map_refresh_response(r: Self::RefreshResponse) -> AuthCredential;
    fn extra_auth_params() -> Vec<(&str, String)>;
}

struct AuthCodeFlowProvider<F: AuthCodeFlow> { ... }
```

Each provider becomes ~150 LOC of `impl AuthCodeFlow`. Saves
~1,700 LOC.

## Why deferred

CLAUDE.md guidance:
> "Don't add features, refactor, or introduce abstractions
> beyond what the task requires. ... Three similar lines is
> better than a premature abstraction."

We have five similar implementations. The pattern is repeated
enough that the abstraction is justified — but the cost of
getting it wrong is high enough that the trigger should be
"adding a 6th provider" or "fixing a bug in 3+ of them at
once," not "noticed during a review pass."

## Trigger for revisiting

1. Adding a 6th OAuth provider — refactor in the same PR.
2. A security or correctness bug found in 2+ providers
   with the same root cause.
3. The user explicitly requests it (with awareness of the
   risk).

Until one of those triggers, this doc is just inventory.
