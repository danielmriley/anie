//! HTTP fetch path: URL validation, SSRF guard, robots.txt
//! caching, per-host rate limiting, bounded HTTP fetching.

use std::{collections::HashMap, net::IpAddr, num::NonZeroU32, sync::Arc, time::Duration};

use async_trait::async_trait;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::keyed::DefaultKeyedStateStore};
use texting_robots::Robot;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::WebToolError;

/// Default user-agent emitted on every fetch. Includes the
/// crate version + the upstream repo so server operators
/// have a clear identification.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "anie/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/danielmriley/anie)"
);

/// Default per-fetch timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default page-size ceiling (10 MiB).
pub const DEFAULT_MAX_BYTES: usize = 10 * 1024 * 1024;

/// Default redirect ceiling.
pub const DEFAULT_MAX_REDIRECTS: usize = 10;

/// Per-host rate limit, requests-per-second.
pub const DEFAULT_RATE_LIMIT_RPS: u32 = 1;

/// Per-host burst capacity. With `DEFAULT_RATE_LIMIT_RPS = 1`,
/// the bucket holds 5 tokens before throttling.
pub const DEFAULT_RATE_LIMIT_BURST: u32 = 5;

/// Default timeout for the headless render path, in seconds.
/// Generous enough for typical SPA hydration; bounded so a
/// hanging page doesn't pin the agent indefinitely.
pub const DEFAULT_HEADLESS_TIMEOUT_SECS: u64 = 30;

/// Maximum bytes captured from a non-2xx response body before
/// truncation. The body is only used for the agent-visible
/// excerpt in [`WebToolError::HttpStatus`], so capping at
/// 256 KiB still gives any reasonable error page room while
/// keeping a misbehaving server from streaming megabytes into
/// memory just because it returned a 500. PR 4.2 of
/// `docs/code_review_2026-04-27/`.
pub const DEFAULT_MAX_ERROR_BODY_BYTES: usize = 256 * 1024;

/// Maximum bytes consumed from a `robots.txt` response. RFC
/// 9309 Section 2.5 suggests 500 KiB as a parser limit; we go
/// slightly higher (512 KiB) to match. A robots.txt larger
/// than this is treated as unavailable rather than parsed —
/// almost certainly the server returning an HTML error page
/// or a malicious endpoint trying to OOM us.
pub const DEFAULT_MAX_ROBOTS_BYTES: usize = 512 * 1024;

// --------------------------------------------------------------
// URL validation + SSRF guard.
// --------------------------------------------------------------

/// Parse and validate a URL, optionally rejecting private /
/// loopback / link-local hosts.
///
/// The host classification uses the URL's textual host. For
/// DNS hostnames that resolve to private IPs at fetch time,
/// the fetch layer re-checks after resolution to defend
/// against DNS rebinding.
pub fn validate_url(raw: &str, allow_private: bool) -> Result<Url, WebToolError> {
    let url = Url::parse(raw).map_err(|e| WebToolError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(WebToolError::UnsupportedScheme(other.into())),
    }
    if !allow_private {
        let host_str = url
            .host_str()
            .ok_or_else(|| WebToolError::InvalidUrl("URL has no host".into()))?;
        if host_str_is_private(host_str) {
            return Err(WebToolError::PrivateAddress(host_str.to_string()));
        }
    }
    Ok(url)
}

/// True if the textual host is a known-private name or a
/// literal private IP. Conservative: returns true for
/// localhost-aliases, RFC 1918 IPv4, IPv6 loopback / unique-
/// local, and `*.local` mDNS names.
fn host_str_is_private(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower.ends_with(".lan")
    {
        return true;
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        return ip_is_private(ip);
    }
    // Bracketed IPv6 literal: [::1].
    if lower.starts_with('[') && lower.ends_with(']') {
        let inner = &lower[1..lower.len() - 1];
        if let Ok(ip) = inner.parse::<IpAddr>() {
            return ip_is_private(ip);
        }
    }
    false
}

/// Classify an IP address as "private" for SSRF purposes.
/// Matches loopback, link-local, RFC 1918 ranges, IPv6 ULA,
/// IPv4 multicast / broadcast, and IPv4-mapped IPv6 addresses
/// whose embedded IPv4 is private.
pub fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                // Carrier-grade NAT (RFC 6598, 100.64.0.0/10).
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000)
                // Class E reserved / future use (240.0.0.0/4).
                // Not officially "private", but no legitimate
                // public host is reachable here, so refusing is
                // strictly safer than allowing.
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped IPv6 (::ffff:a.b.c.d). `is_loopback`
            // and friends only check the canonical IPv6 ranges
            // and miss e.g. ::ffff:127.0.0.1, so classify by the
            // embedded IPv4 instead. Without this, an attacker
            // who can spoof a `Location: http://[::ffff:127.0.0.1]`
            // redirect would slip past the v6 checks below.
            // PR 3.2 of `docs/code_review_2026-04-27/`.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ip_is_private(IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique local addresses (fc00::/7).
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // Link-local (fe80::/10).
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

// --------------------------------------------------------------
// DNS resolver abstraction.
// --------------------------------------------------------------

/// DNS resolver abstraction used by the SSRF guard.
///
/// `validate_url()` covers IP literals and known private
/// hostnames (`localhost`, `*.local`, `*.internal`, etc.) but
/// does not catch arbitrary public-looking hostnames that
/// resolve to private IPs (e.g. `evil.example` →
/// `127.0.0.1`, or AWS metadata via a CNAME). The fetch path
/// therefore resolves hostnames before issuing the request and
/// rejects when any candidate IP is private.
///
/// A trait abstraction lets tests inject deterministic
/// mappings without touching the system resolver. The default
/// implementation is [`SystemResolver`], which delegates to
/// `tokio::net::lookup_host`.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Resolve `host:port` to its candidate IP addresses.
    /// `port` is included so callers can use `(host, port)`
    /// resolution APIs without constructing a `SocketAddr`.
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, WebToolError>;
}

/// Default resolver: delegates to `tokio::net::lookup_host`,
/// which uses the platform-configured DNS.
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, WebToolError> {
        let addrs = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| WebToolError::Fetch(format!("dns lookup of {host} failed: {e}")))?;
        let ips: Vec<IpAddr> = addrs.map(|sa| sa.ip()).collect();
        Ok(ips)
    }
}

/// Convenience constructor for the default system resolver as
/// an `Arc<dyn Resolver>`. Production call sites pass this; tests
/// substitute their own resolver via the same trait.
pub fn system_resolver() -> Arc<dyn Resolver> {
    Arc::new(SystemResolver)
}

/// Validate a URL's destination against the SSRF policy.
///
/// `validate_url` covers the textual host (literal IPs and
/// known-private hostnames). This helper additionally resolves
/// non-literal hostnames and rejects when any returned IP is
/// classified as private by [`ip_is_private`]. The redirect
/// loop in [`fetch_html`] calls this before sending each
/// request so a 302 to a hostname that resolves to a private
/// IP cannot bypass the guard.
///
/// anie-specific (vs. a connector-integrated resolver): there
/// is a small TOCTOU between this validation and the request
/// reqwest issues — DNS could in principle change between the
/// two lookups. Pi has no equivalent guard at all, so this is a
/// strict improvement; closing the TOCTOU requires a custom
/// reqwest `Resolve` implementation and is tracked as a
/// follow-up. PR 3.2 of `docs/code_review_2026-04-27/`.
pub async fn validate_destination(
    url: &Url,
    resolver: &dyn Resolver,
    allow_private_ips: bool,
) -> Result<(), WebToolError> {
    if allow_private_ips {
        return Ok(());
    }
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)) => {
            // Literal IPs were already classified by
            // `validate_url`. There is no DNS step to re-check.
            return Ok(());
        }
        None => {
            return Err(WebToolError::InvalidUrl("URL has no host".into()));
        }
    };
    let port = url.port_or_known_default().unwrap_or(0);
    let ips = resolver.resolve(&host, port).await?;
    if ips.is_empty() {
        return Err(WebToolError::Fetch(format!(
            "dns lookup of {host} returned no addresses"
        )));
    }
    for ip in &ips {
        if ip_is_private(*ip) {
            return Err(WebToolError::PrivateAddress(format!("{host} -> {ip}")));
        }
    }
    Ok(())
}

// --------------------------------------------------------------
// robots.txt cache.
// --------------------------------------------------------------

/// In-memory robots.txt cache. Parses each host's robots.txt
/// once and reuses the result for the lifetime of the cache.
///
/// The cache is intentionally simple — no TTL, no persistence.
/// A long-running anie session that hits the same hosts many
/// times benefits; one-shot CLI invocations re-fetch each
/// time, which is fine.
#[derive(Clone, Default)]
pub struct RobotsCache {
    inner: Arc<RwLock<HashMap<String, Option<Robot>>>>,
}

impl RobotsCache {
    /// Build an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether `url` may be fetched under `user_agent`.
    /// Returns `Ok(())` if allowed (or robots.txt unavailable);
    /// `Err(WebToolError::Forbidden)` if explicitly disallowed.
    /// Honors `cancel` for the robots.txt fetch on the slow
    /// path; the in-memory cache lookup is uncancellable but
    /// effectively instantaneous.
    pub async fn check(
        &self,
        url: &Url,
        user_agent: &str,
        client: &reqwest::Client,
        cancel: &CancellationToken,
    ) -> Result<(), WebToolError> {
        let host = url
            .host_str()
            .ok_or_else(|| WebToolError::InvalidUrl("URL has no host".into()))?
            .to_string();

        // Fast path: cached.
        {
            let cache = self.inner.read().await;
            if let Some(slot) = cache.get(&host) {
                return self.evaluate(slot.as_ref(), url, user_agent);
            }
        }

        // Slow path: fetch robots.txt for this host.
        let robots = tokio::select! {
            _ = cancel.cancelled() => return Err(WebToolError::Aborted),
            r = fetch_robots_for(client, url) => r,
        };
        let mut cache = self.inner.write().await;
        let slot = cache.entry(host).or_insert(robots);
        self.evaluate(slot.as_ref(), url, user_agent)
    }

    fn evaluate(
        &self,
        robot: Option<&Robot>,
        url: &Url,
        _user_agent: &str,
    ) -> Result<(), WebToolError> {
        match robot {
            None => Ok(()), // No robots.txt → permissive.
            Some(robot) => {
                if robot.allowed(url.as_str()) {
                    Ok(())
                } else {
                    Err(WebToolError::Forbidden(url.to_string()))
                }
            }
        }
    }

    /// Test-only insert.
    #[cfg(test)]
    pub async fn insert(&self, host: &str, robot: Option<Robot>) {
        let mut cache = self.inner.write().await;
        cache.insert(host.to_string(), robot);
    }
}

async fn fetch_robots_for(client: &reqwest::Client, url: &Url) -> Option<Robot> {
    let mut robots_url = url.clone();
    robots_url.set_path("/robots.txt");
    robots_url.set_query(None);
    robots_url.set_fragment(None);

    let response = client
        .get(robots_url.clone())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    // Bound the read. A robots.txt larger than the cap is
    // treated as unavailable rather than truncated-and-parsed
    // — silently parsing a partial file could miss a
    // `Disallow:` directive at the bottom and we'd let the
    // crawl through a path the operator forbade. Better to
    // fail closed (no robots data → permissive evaluation
    // already in `RobotsCache::evaluate`) than partially
    // open. PR 4.2 of `docs/code_review_2026-04-27/`.
    let (body, overflowed) = collect_bounded_body(response, DEFAULT_MAX_ROBOTS_BYTES)
        .await
        .ok()?;
    if overflowed {
        tracing::warn!(
            url = %robots_url,
            cap = DEFAULT_MAX_ROBOTS_BYTES,
            "robots.txt exceeds cap; treating as unavailable"
        );
        return None;
    }
    Robot::new("*", &body).ok()
}

/// Drain a non-2xx response body into a UTF-8 string, capped
/// at [`DEFAULT_MAX_ERROR_BODY_BYTES`]. Honors `cancel` between
/// chunks. Used by `fetch_html` to build the excerpt carried in
/// [`WebToolError::HttpStatus`] without giving a misbehaving
/// server an OOM vector via the error path.
async fn bounded_text_for_error(
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<String, WebToolError> {
    use futures::stream::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut overflowed = false;
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(WebToolError::Aborted),
            n = stream.next() => n,
        };
        let Some(chunk) = next else { break };
        // Drain failures here are best-effort: the response is
        // already a non-success and we just want an excerpt.
        // Treat a stream error as end-of-body rather than a
        // hard fetch error.
        let Ok(chunk) = chunk else { break };
        let remaining = DEFAULT_MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            overflowed = true;
        } else if !overflowed {
            buf.extend_from_slice(&chunk);
        }
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if overflowed {
        text.push_str("\n…[truncated]");
    }
    Ok(text)
}

/// Stream a response body into a `Vec<u8>`, capped at `cap`
/// bytes. Returns `(body, overflowed)` — `overflowed == true`
/// means the source produced more bytes than `cap` and only
/// the first `cap` bytes were kept. The underlying stream is
/// drained to completion either way, so the connection is
/// returned to the pool cleanly. PR 4.2 of
/// `docs/code_review_2026-04-27/`.
async fn collect_bounded_body(
    response: reqwest::Response,
    cap: usize,
) -> Result<(Vec<u8>, bool), WebToolError> {
    use futures::stream::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut overflowed = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| WebToolError::Fetch(e.to_string()))?;
        let remaining = cap.saturating_sub(buf.len());
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            overflowed = true;
            // Keep draining so the pipe doesn't backpressure
            // a server that's still streaming. Subsequent
            // chunks are dropped.
        } else if !overflowed {
            buf.extend_from_slice(&chunk);
        }
    }
    Ok((buf, overflowed))
}

// --------------------------------------------------------------
// Per-host rate limiter.
// --------------------------------------------------------------

/// Token-bucket rate limiter keyed by host.
///
/// Wraps `governor::RateLimiter` with a host-string key. Each
/// host gets its own bucket; cross-host fetches don't compete.
pub struct HostRateLimiter {
    limiter: RateLimiter<String, DefaultKeyedStateStore<String>, DefaultClock>,
}

impl HostRateLimiter {
    /// Build a rate limiter with `rps` requests per second per
    /// host and a burst of `burst` tokens.
    pub fn new(rps: u32, burst: u32) -> Self {
        // `max(1)` plus `unwrap_or` keeps the constructor
        // total without a panic path. Caller passing 0 gets
        // a 1-rps / 1-burst limiter, not a crash.
        let rps = NonZeroU32::new(rps.max(1)).unwrap_or(NonZeroU32::MIN);
        let burst = NonZeroU32::new(burst.max(1)).unwrap_or(NonZeroU32::MIN);
        let quota = Quota::per_second(rps).allow_burst(burst);
        Self {
            limiter: RateLimiter::keyed(quota),
        }
    }

    /// Wait until a token is available for `host`. The wait is
    /// driven by Tokio's timer so concurrent calls don't block
    /// the executor.
    pub async fn acquire(&self, host: &str) {
        let key = host.to_string();
        self.limiter.until_key_ready(&key).await;
    }

    /// Try to acquire without waiting. Returns `true` on
    /// success (token consumed), `false` when throttled.
    /// Used by tests.
    pub fn try_acquire(&self, host: &str) -> bool {
        self.limiter.check_key(&host.to_string()).is_ok()
    }
}

impl Default for HostRateLimiter {
    fn default() -> Self {
        Self::new(DEFAULT_RATE_LIMIT_RPS, DEFAULT_RATE_LIMIT_BURST)
    }
}

// --------------------------------------------------------------
// Bounded HTTP fetching.
// --------------------------------------------------------------

/// Fetch options threaded through `fetch_html`.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub timeout: Duration,
    pub user_agent: String,
    pub max_bytes: usize,
    pub max_redirects: usize,
    pub allow_private_ips: bool,
    /// Total budget for `javascript: true` renders. Only used
    /// when the crate is built with `--features headless`.
    pub headless_timeout_secs: u64,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            user_agent: DEFAULT_USER_AGENT.into(),
            max_bytes: DEFAULT_MAX_BYTES,
            max_redirects: DEFAULT_MAX_REDIRECTS,
            allow_private_ips: false,
            headless_timeout_secs: DEFAULT_HEADLESS_TIMEOUT_SECS,
        }
    }
}

/// Build a `reqwest::Client` with sane defaults for the web
/// tools. Reused across calls for connection pooling.
///
/// Redirects are disabled at the reqwest layer
/// (`Policy::none()`) so `fetch_html` can validate every
/// redirect target against `validate_url` before issuing the
/// next request. `Policy::limited` would let `reqwest` follow
/// a 302 to `http://127.0.0.1` before anie ever saw the
/// `Location` header — exactly the SSRF bypass the manual
/// redirect loop in `fetch_html` is designed to prevent.
/// PR 3.1 of `docs/code_review_2026-04-27/`.
pub fn build_client(opts: &FetchOptions) -> Result<reqwest::Client, WebToolError> {
    reqwest::Client::builder()
        .timeout(opts.timeout)
        .user_agent(&opts.user_agent)
        .redirect(reqwest::redirect::Policy::none())
        .gzip(true)
        .brotli(true)
        .build()
        .map_err(|e| WebToolError::Fetch(e.to_string()))
}

/// Fetch the HTML body at `url`, capped at `opts.max_bytes`.
/// Streams the response and bails the moment the byte counter
/// exceeds the cap, so a hostile server claiming a small
/// `Content-Length` then streaming gigabytes can't OOM us.
///
/// Manual redirect handling — `build_client` disables
/// `reqwest`'s automatic redirects so this function can call
/// `validate_url` on every `Location` target before sending
/// the next request. Without manual handling, a server could
/// 302 us into `http://127.0.0.1/admin` and `reqwest` would
/// happily follow before anie's SSRF guard had a chance to
/// run. PR 3.1 of `docs/code_review_2026-04-27/`.
///
/// SSRF DNS guard — every URL in the chain (initial and each
/// redirect target) goes through [`validate_destination`],
/// which resolves the hostname and rejects when any candidate
/// IP is private. `validate_url` only classifies the textual
/// host; without the DNS step, a public-looking hostname like
/// `evil.example` could resolve to `127.0.0.1` and slip
/// through. PR 3.2 of `docs/code_review_2026-04-27/`.
pub async fn fetch_html(
    client: &reqwest::Client,
    resolver: &dyn Resolver,
    cancel: &CancellationToken,
    url: &Url,
    opts: &FetchOptions,
) -> Result<String, WebToolError> {
    if cancel.is_cancelled() {
        return Err(WebToolError::Aborted);
    }
    // Initial DNS check. `validate_url` was called by the
    // caller; we add the resolved-IP classification here.
    tokio::select! {
        _ = cancel.cancelled() => return Err(WebToolError::Aborted),
        r = validate_destination(url, resolver, opts.allow_private_ips) => r?,
    }

    let mut current = url.clone();
    let mut hops = 0usize;
    let response = loop {
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(WebToolError::Aborted),
            r = client.get(current.clone()).send() => r
                .map_err(|e| WebToolError::Fetch(e.to_string()))?,
        };

        let status = response.status();
        if !status.is_redirection() {
            break response;
        }

        if hops >= opts.max_redirects {
            return Err(WebToolError::Fetch(format!(
                "exceeded max_redirects ({}) following {url}",
                opts.max_redirects,
            )));
        }
        hops += 1;

        let Some(location) = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
        else {
            // 3xx without Location is malformed — surface as
            // an HTTP error like any other unsuccessful
            // status. Drain a bounded excerpt of the body for
            // the agent.
            let body = bounded_text_for_error(response, cancel).await?;
            return Err(WebToolError::HttpStatus {
                code: status.as_u16(),
                body_excerpt: WebToolError::truncate_excerpt(&body),
            });
        };

        // Resolve relative redirects against the current URL.
        let next = current
            .join(location)
            .map_err(|e| WebToolError::Fetch(format!("redirect target invalid: {e}")))?;

        // Validate the next target with the same SSRF rules
        // as the initial URL. The body of the redirect
        // response is intentionally dropped — its content
        // doesn't matter once we've classified the
        // destination.
        let _ = response;
        current = validate_url(next.as_str(), opts.allow_private_ips)?;
        tokio::select! {
            _ = cancel.cancelled() => return Err(WebToolError::Aborted),
            r = validate_destination(&current, resolver, opts.allow_private_ips) => r?,
        }
    };

    let status = response.status();
    if !status.is_success() {
        // Try to capture a body excerpt for the agent to see.
        // Bounded so a hostile server returning a 500 + 50 MiB
        // body can't OOM us via the error path. The displayed
        // excerpt is then further trimmed by `truncate_excerpt`
        // for the agent.
        let body = bounded_text_for_error(response, cancel).await?;
        return Err(WebToolError::HttpStatus {
            code: status.as_u16(),
            body_excerpt: WebToolError::truncate_excerpt(&body),
        });
    }

    // Defense in depth: even though every URL in the chain
    // was validated against `allow_private_ips`, double-check
    // the final response's host. Catches any future regression
    // where the redirect loop accepts a target it shouldn't.
    if !opts.allow_private_ips
        && let Some(final_host) = response.url().host_str()
        && host_str_is_private(final_host)
    {
        return Err(WebToolError::PrivateAddress(final_host.to_string()));
    }

    // Reject non-HTML responses up front. Defuddle's HTML parser
    // crashes with `Cannot destructure property 'firstElementChild'
    // of 'documentElement' as it is null` when fed plain text or
    // JSON (caught by smoke runs against `wttr.in` and a Yahoo
    // weather endpoint). A typed error here is much better than a
    // confusing Defuddle stack trace.
    if let Some(ct) = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    {
        let lower = ct.to_ascii_lowercase();
        let is_html = lower.starts_with("text/html")
            || lower.starts_with("application/xhtml")
            || lower.starts_with("application/xml")
            || lower.starts_with("text/xml");
        if !is_html {
            return Err(WebToolError::UnsupportedContentType(ct.to_string()));
        }
    }

    // Stream the body, enforcing the size cap as we go.
    // The chunk-level cancellation check matters: a slow
    // server feeding us bytes drip-by-drip would otherwise
    // pin the agent until the timeout fires.
    use futures::stream::StreamExt;
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut stream = response.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(WebToolError::Aborted),
            n = stream.next() => n,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| WebToolError::Fetch(e.to_string()))?;
        if buf.len() + chunk.len() > opts.max_bytes {
            return Err(WebToolError::TooLarge {
                bytes: buf.len() + chunk.len(),
                max: opts.max_bytes,
            });
        }
        buf.extend_from_slice(&chunk);
    }

    String::from_utf8(buf).or_else(|err| {
        // Lossy fallback: the bytes weren't valid UTF-8, but
        // most HTML parsers handle this. Defuddle is happy
        // with replacement-char-decoded input.
        Ok(String::from_utf8_lossy(err.as_bytes()).into_owned())
    })
}

// --------------------------------------------------------------
// Tests
// --------------------------------------------------------------

// --------------------------------------------------------------
// Test resolver. Public so integration tests in
// `tests/fetch_basic.rs` (which see only the crate's public
// surface) can model "hostname maps to private IP" — the
// attack we're guarding against — without touching the system
// DNS resolver.
// --------------------------------------------------------------

/// Static `Resolver` for tests and dev tooling: returns canned
/// IPs per hostname. Doc-hidden because production code should
/// always use [`SystemResolver`]; this exists so the SSRF guard
/// can be exercised deterministically.
#[doc(hidden)]
pub struct StaticResolver {
    map: HashMap<String, Vec<IpAddr>>,
}

impl StaticResolver {
    /// Build a resolver from `(host, ips)` pairs.
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = (S, Vec<IpAddr>)>,
        S: Into<String>,
    {
        Self {
            map: entries
                .into_iter()
                .map(|(h, ips)| (h.into(), ips))
                .collect(),
        }
    }
}

#[async_trait]
impl Resolver for StaticResolver {
    async fn resolve(&self, host: &str, _port: u16) -> Result<Vec<IpAddr>, WebToolError> {
        match self.map.get(host) {
            Some(ips) => Ok(ips.clone()),
            None => Err(WebToolError::Fetch(format!(
                "static resolver has no mapping for {host}"
            ))),
        }
    }
}

// --------------------------------------------------------------
// Tests
// --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://example.com/path", false).is_ok());
    }

    #[test]
    fn validate_url_accepts_http() {
        assert!(validate_url("http://example.com/path", false).is_ok());
    }

    #[test]
    fn validate_url_rejects_file_scheme() {
        let err = validate_url("file:///etc/passwd", false).unwrap_err();
        assert!(matches!(err, WebToolError::UnsupportedScheme(_)));
    }

    #[test]
    fn validate_url_rejects_javascript_scheme() {
        let err = validate_url("javascript:alert(1)", false).unwrap_err();
        assert!(matches!(err, WebToolError::UnsupportedScheme(_)));
    }

    #[test]
    fn validate_url_rejects_loopback_when_private_disallowed() {
        let err = validate_url("http://127.0.0.1/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[test]
    fn validate_url_rejects_localhost_when_private_disallowed() {
        let err = validate_url("http://localhost/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[test]
    fn validate_url_rejects_rfc1918_when_private_disallowed() {
        let err = validate_url("http://10.0.0.1/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
        let err = validate_url("http://192.168.1.1/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
        let err = validate_url("http://172.16.0.1/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[test]
    fn validate_url_rejects_ipv6_loopback() {
        let err = validate_url("http://[::1]/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[test]
    fn validate_url_rejects_local_mdns() {
        let err = validate_url("http://printer.local/", false).unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[test]
    fn validate_url_allows_private_when_explicitly_enabled() {
        assert!(validate_url("http://localhost/", true).is_ok());
        assert!(validate_url("http://10.0.0.1/", true).is_ok());
        assert!(validate_url("http://[::1]/", true).is_ok());
    }

    #[test]
    fn validate_url_rejects_malformed() {
        let err = validate_url("not a url", false).unwrap_err();
        assert!(matches!(err, WebToolError::InvalidUrl(_)));
    }

    #[test]
    fn ip_is_private_classifies_known_ranges() {
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        // CGNAT 100.64.0.0/10.
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        // EC2 metadata: link-local, must be private.
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))));
        // Class E reserved (240.0.0.0/4): no legitimate public
        // host lives here, treat as private.
        assert!(ip_is_private(IpAddr::V4(Ipv4Addr::new(240, 0, 0, 1))));
        // Public IP example: not private.
        assert!(!ip_is_private(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    }

    /// Regression for PR 3.2: without the IPv4-mapped path,
    /// `Ipv6Addr::is_loopback` only matches `::1` and a server
    /// could redirect to `[::ffff:127.0.0.1]` to reach the
    /// loopback interface unflagged.
    #[test]
    fn ip_is_private_classifies_ipv4_mapped_ipv6_by_embedded_v4() {
        // ::ffff:127.0.0.1 — IPv4-mapped loopback.
        let mapped_loopback = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001));
        assert!(ip_is_private(mapped_loopback));

        // ::ffff:10.0.0.1 — IPv4-mapped RFC 1918.
        let mapped_rfc1918 = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0001));
        assert!(ip_is_private(mapped_rfc1918));

        // ::ffff:8.8.8.8 — IPv4-mapped public IP. Must NOT
        // classify as private; the attack and its mitigation
        // should be symmetric.
        let mapped_public = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x0808, 0x0808));
        assert!(!ip_is_private(mapped_public));
    }

    #[tokio::test]
    async fn validate_destination_rejects_hostname_resolving_to_loopback() {
        let resolver = StaticResolver::new(vec![(
            "evil.example",
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        )]);
        let url = Url::parse("http://evil.example/page").unwrap();
        let err = validate_destination(&url, &resolver, false)
            .await
            .unwrap_err();
        match err {
            WebToolError::PrivateAddress(msg) => {
                assert!(msg.contains("evil.example"), "got: {msg}");
                assert!(msg.contains("127.0.0.1"), "got: {msg}");
            }
            other => panic!("expected PrivateAddress, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_destination_rejects_hostname_resolving_to_link_local_metadata() {
        // EC2/GCP metadata service IP. Hostname that resolves
        // here is one of the most-commonly-cited SSRF targets.
        let resolver = StaticResolver::new(vec![(
            "metadata.example",
            vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))],
        )]);
        let url = Url::parse("http://metadata.example/latest").unwrap();
        let err = validate_destination(&url, &resolver, false)
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[tokio::test]
    async fn validate_destination_rejects_when_any_resolved_ip_is_private() {
        // Hostnames returning a mix of public and private IPs
        // (round-robin DNS, A+AAAA records pointing at
        // different segments) must reject as a unit. Allowing
        // through "if at least one IP is public" would race
        // reqwest's connect order against the SSRF guard.
        let resolver = StaticResolver::new(vec![(
            "split.example",
            vec![
                IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
            ],
        )]);
        let url = Url::parse("http://split.example/").unwrap();
        let err = validate_destination(&url, &resolver, false)
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::PrivateAddress(_)));
    }

    #[tokio::test]
    async fn validate_destination_allows_public_resolution() {
        let resolver = StaticResolver::new(vec![(
            "good.example",
            vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))],
        )]);
        let url = Url::parse("http://good.example/").unwrap();
        validate_destination(&url, &resolver, false)
            .await
            .expect("public IP must pass");
    }

    #[tokio::test]
    async fn validate_destination_skips_dns_when_private_explicitly_allowed() {
        // `allow_private_ips = true` is the operator opt-in;
        // the DNS check (and any resolver error) must not
        // fire. Simulate by handing in a resolver that would
        // panic if called — if the early-return path breaks,
        // this test will surface it.
        struct PanicResolver;
        #[async_trait]
        impl Resolver for PanicResolver {
            async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, WebToolError> {
                panic!("resolver must not be called when allow_private_ips=true");
            }
        }
        let url = Url::parse("http://good.example/").unwrap();
        validate_destination(&url, &PanicResolver, true)
            .await
            .expect("allow_private_ips=true bypasses DNS");
    }

    #[tokio::test]
    async fn validate_destination_skips_dns_for_ip_literals() {
        // Literal IPs were already classified by `validate_url`.
        // `validate_destination` must not re-resolve them
        // through DNS — that would only invent a TOCTOU window
        // for nothing. Same panic-resolver trick.
        struct PanicResolver;
        #[async_trait]
        impl Resolver for PanicResolver {
            async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, WebToolError> {
                panic!("resolver must not be called for IP literals");
            }
        }
        let url = Url::parse("http://8.8.8.8/").unwrap();
        validate_destination(&url, &PanicResolver, false)
            .await
            .expect("IP literal: no DNS step");
    }

    /// PR 4.2 of `docs/code_review_2026-04-27/`. A
    /// `bytes_stream` consumer must keep memory bounded even
    /// when the response is many times larger than the cap.
    #[tokio::test]
    async fn collect_bounded_body_caps_at_size() {
        // Build a fake server returning a 4 MiB body.
        use httpmock::Method::GET;
        use httpmock::MockServer;

        let server = MockServer::start_async().await;
        let big = vec![b'x'; 4 * 1024 * 1024];
        server
            .mock_async(|when, then| {
                when.method(GET).path("/big");
                then.status(200).body(big);
            })
            .await;

        let response = reqwest::get(format!("{}/big", server.base_url()))
            .await
            .expect("get");
        let (buf, overflowed) = collect_bounded_body(response, 64 * 1024).await.expect("ok");
        assert_eq!(buf.len(), 64 * 1024);
        assert!(overflowed);
    }

    #[tokio::test]
    async fn validate_destination_rejects_empty_resolution() {
        struct EmptyResolver;
        #[async_trait]
        impl Resolver for EmptyResolver {
            async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, WebToolError> {
                Ok(Vec::new())
            }
        }
        let url = Url::parse("http://nx.example/").unwrap();
        let err = validate_destination(&url, &EmptyResolver, false)
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::Fetch(_)));
    }

    #[test]
    fn rate_limiter_allows_burst_then_throttles() {
        let limiter = HostRateLimiter::new(1, 3);
        // First three calls succeed (burst = 3).
        assert!(limiter.try_acquire("example.com"));
        assert!(limiter.try_acquire("example.com"));
        assert!(limiter.try_acquire("example.com"));
        // Fourth call without waiting: throttled.
        assert!(!limiter.try_acquire("example.com"));
    }

    #[test]
    fn rate_limiter_keys_by_host_independently() {
        let limiter = HostRateLimiter::new(1, 1);
        // Each host gets its own bucket.
        assert!(limiter.try_acquire("example.com"));
        assert!(limiter.try_acquire("other.com"));
        // Same host throttled.
        assert!(!limiter.try_acquire("example.com"));
    }

    #[tokio::test]
    async fn rate_limiter_recovers_after_window() {
        // 100 rps gives a 10ms-ish window between tokens with
        // burst 1; we wait 50ms to be safe.
        let limiter = HostRateLimiter::new(100, 1);
        assert!(limiter.try_acquire("example.com"));
        assert!(!limiter.try_acquire("example.com"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(limiter.try_acquire("example.com"));
    }

    #[tokio::test]
    async fn robots_cache_returns_ok_when_no_robots_txt() {
        let cache = RobotsCache::new();
        cache.insert("example.com", None).await;
        let url = Url::parse("https://example.com/page").unwrap();
        let client = reqwest::Client::new();
        assert!(
            cache
                .check(&url, "anie/test", &client, &CancellationToken::new())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn robots_cache_disallows_when_robot_says_so() {
        let robot = Robot::new("anie/test", b"User-agent: *\nDisallow: /private/").unwrap();
        let cache = RobotsCache::new();
        cache.insert("example.com", Some(robot)).await;
        let url = Url::parse("https://example.com/private/secret").unwrap();
        let client = reqwest::Client::new();
        let err = cache
            .check(&url, "anie/test", &client, &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, WebToolError::Forbidden(_)));
    }

    #[tokio::test]
    async fn robots_cache_allows_unrestricted_paths() {
        let robot = Robot::new("anie/test", b"User-agent: *\nDisallow: /private/").unwrap();
        let cache = RobotsCache::new();
        cache.insert("example.com", Some(robot)).await;
        let url = Url::parse("https://example.com/public/article").unwrap();
        let client = reqwest::Client::new();
        assert!(
            cache
                .check(&url, "anie/test", &client, &CancellationToken::new())
                .await
                .is_ok()
        );
    }

    #[test]
    fn truncate_excerpt_caps_long_strings() {
        let long = "x".repeat(1000);
        let trimmed = WebToolError::truncate_excerpt(&long);
        assert!(trimmed.chars().count() <= 257); // 256 + ellipsis
        assert!(trimmed.ends_with('…'));
    }

    #[test]
    fn truncate_excerpt_passes_short_strings_through() {
        let short = "hello";
        assert_eq!(WebToolError::truncate_excerpt(short), short);
    }
}
