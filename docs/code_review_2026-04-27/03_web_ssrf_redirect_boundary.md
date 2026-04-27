# 03 — Web SSRF and redirect boundary

## Rationale

The web fetch code advertises a DNS-time SSRF defense but currently only
checks textual hosts:

- `crates/anie-tools-web/src/read/fetch.rs:53-74` — `validate_url()`
  rejects private literal hosts and local hostnames.
- `crates/anie-tools-web/src/read/fetch.rs:56-59` — comment claims DNS
  hostnames resolving to private IPs are re-checked at fetch time.
- `crates/anie-tools-web/src/read/fetch.rs:308-315` — client enables
  `Policy::limited(opts.max_redirects)` automatic redirects.
- `crates/anie-tools-web/src/read/fetch.rs:334-350` — final-host privacy
  check happens after the request/redirect has already been sent.
- `crates/anie-tools-web/src/read/tool.rs:107-115` and
  `crates/anie-tools-web/src/read/headless.rs:82-155` — the headless
  path hands navigation to Chrome after the initial URL validation.

For a default-enabled live-web tool, this is the security-critical
finding from the review.

## Design

Make the non-headless HTTP fetch path enforce this invariant:

> When `allow_private_ips == false`, no HTTP request is sent to a
> loopback, private, link-local, multicast, unspecified, local-name, or
> otherwise disallowed destination — including redirects.

Implementation principles:

1. **Validate before every request.** Initial URL and every redirect
   target must be checked before issuing the next request.
2. **Disable automatic redirects.** `reqwest` must not follow a redirect
   before anie can validate the `Location`.
3. **Validate resolved socket IPs.** Hostname checks are not enough.
   The fetch path must classify the IPs returned for the target host.
4. **Keep operator escape hatch explicit.** `FetchOptions::allow_private_ips`
   already exists. Tests must prove it bypasses the private-IP rejection
   only when intentionally enabled.
5. **Treat headless as a separate boundary.** Chrome can follow redirects
   and load subresources outside Rust's `fetch_html`. Until request
   interception is implemented, the headless path should be documented
   as not equivalent to the SSRF-hardened non-headless path, and the
   safest default should be chosen.

## Files to touch

- `crates/anie-tools-web/src/read/fetch.rs`
  - Disable automatic redirects in `build_client()`.
  - Add manual redirect loop inside `fetch_html()`.
  - Add pre-request destination validation that includes DNS resolution.
  - Extend `ip_is_private()` for IPv4-mapped IPv6 and other special
    ranges if missing.
- `crates/anie-tools-web/src/read/tool.rs`
  - Preserve existing `allow_private_ips` behavior.
  - Make headless policy explicit.
- `crates/anie-tools-web/src/read/headless.rs`
  - Optional: add request interception in a later PR.
- `crates/anie-tools-web/tests/fetch_basic.rs`
  - Redirect regression tests.
- `crates/anie-tools-web/src/read/fetch.rs` tests
  - Unit tests for IP classification and URL/redirect validation.

## Phased PRs

### PR A — Manual redirect validation

**Change:**

- Change `build_client()` to `redirect(reqwest::redirect::Policy::none())`.
- Implement redirect handling in `fetch_html()`:
  - Start with the validated URL.
  - Send a request.
  - For `3xx` with `Location`, resolve relative targets against the
    current URL.
  - Validate the next URL before sending the next request.
  - Stop at `opts.max_redirects` and return a typed error if exceeded.
- Preserve existing success/error/content-type behavior after the final
  response.

**Tests:**

- `fetch_follows_redirect_chain` still passes under manual redirects.
- `fetch_caps_redirect_chain` still passes.
- New regression: public fixture URL redirects to `http://127.0.0.1/...`;
  the private target is rejected before the private endpoint observes a
  request.
- New regression: relative redirect to private literal is also rejected.

**Exit criteria:**

- No automatic redirect can bypass `validate_url()`.

### PR B — DNS/resolved-IP validation before connect

**Change:**

- Add a small resolver abstraction used by `fetch_html()` so tests can
  inject host→IP mappings deterministically.
- For hostnames, resolve `(host, port)` before the request and reject if
  any candidate IP is private when `allow_private_ips == false`.
- Prefer a request-time resolver/connector integration if feasible, so
  DNS cannot change between validation and connect. If reqwest's custom
  resolver API is too invasive for this PR, land pre-connect validation
  with an inline `anie-specific` comment documenting the remaining TOCTOU
  and track a follow-up.

**Tests:**

- Hostname resolving to `127.0.0.1` is rejected with
  `WebToolError::PrivateAddress`.
- Hostname resolving to `169.254.169.254` is rejected.
- Public IP resolution is allowed.
- `allow_private_ips = true` allows the same private resolution.
- IPv4-mapped IPv6 loopback/private cases are classified private.

**Exit criteria:**

- The review comment at `fetch.rs:56-59` is true for the non-headless
  path, or the remaining TOCTOU is explicitly documented and tested as
  a known limitation.

### PR C — Headless path policy and interception plan

**Change:** choose one of these safe shapes:

1. **Conservative near-term:** keep `javascript=true` feature-gated and
   document that it is not SSRF-equivalent to `fetch_html()`. Reject
   `javascript=true` unless a new explicit config knob enables the
   less-restricted Chrome navigation path.
2. **Stronger implementation:** use Chrome DevTools request interception
   to reject private/loopback/link-local URLs for main-frame redirects
   and subresources before Chrome sends them.

Given persistent-agent goals, option 1 can land quickly to keep the
surface explicit; option 2 can follow when tested.

**Tests:**

- Default build still rejects `javascript=true` without `headless`.
- Headless-enabled build documents/guards private literal URLs before
  launch.
- If interception lands, add a mocked page that attempts to load a
  private subresource and prove it is aborted.

**Exit criteria:**

- Users/operators can tell whether `javascript=true` is SSRF-hardened or
  an explicitly enabled escape hatch.

## Test plan

- `cargo test -p anie-tools-web`
- `cargo test -p anie-tools-web --features headless` where practical.
- `cargo check -p anie-cli --features web-headless`
- Manual smoke:
  - Public page fetch succeeds.
  - Public URL redirecting to loopback is blocked before loopback sees
    the request.
  - A known public redirect chain still succeeds.

## Risks

- DNS validation without connector integration leaves a DNS-rebinding
  TOCTOU. If a full resolver integration is too large, document the
  limitation inline and make the follow-up explicit.
- Redirect handling must preserve method semantics. `web_read` only uses
  GET today, so this is simpler than a general HTTP client.
- Some legitimate public sites use redirects through tracking hosts.
  Manual redirect handling must preserve relative URL resolution and
  common 301/302/303/307/308 behavior for GET.

## Exit criteria

- Non-headless `web_read` does not send requests to private destinations
  unless `allow_private_ips` is explicitly true.
- Redirects are validated before follow-up requests.
- DNS/private-IP behavior has deterministic tests.
- Headless security posture is explicit and not implied by the
  non-headless guarantee.

## Deferred

- Full browser sandboxing for Chrome. Request interception is enough for
  this finding; process/container sandboxing is a broader tool-isolation
  project.
