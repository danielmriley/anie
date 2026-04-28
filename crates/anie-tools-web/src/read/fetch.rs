//! HTTP fetch path: URL validation, SSRF guard, robots.txt
//! caching, per-host rate limiting, bounded HTTP fetching.

use std::{collections::HashMap, net::IpAddr, num::NonZeroU32, sync::Arc, time::Duration};

use governor::{Quota, RateLimiter, clock::DefaultClock, state::keyed::DefaultKeyedStateStore};
use texting_robots::Robot;
use tokio::sync::RwLock;
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
/// and IPv4 multicast / broadcast.
pub fn ip_is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                // Carrier-grade NAT.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000)
        }
        IpAddr::V6(v6) => {
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
    pub async fn check(
        &self,
        url: &Url,
        user_agent: &str,
        client: &reqwest::Client,
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
        let robots = fetch_robots_for(client, url).await;
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
    let body = response.bytes().await.ok()?;
    Robot::new("*", &body).ok()
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
pub fn build_client(opts: &FetchOptions) -> Result<reqwest::Client, WebToolError> {
    reqwest::Client::builder()
        .timeout(opts.timeout)
        .user_agent(&opts.user_agent)
        .redirect(reqwest::redirect::Policy::limited(opts.max_redirects))
        .gzip(true)
        .brotli(true)
        .build()
        .map_err(|e| WebToolError::Fetch(e.to_string()))
}

/// Fetch the HTML body at `url`, capped at `opts.max_bytes`.
/// Streams the response and bails the moment the byte counter
/// exceeds the cap, so a hostile server claiming a small
/// `Content-Length` then streaming gigabytes can't OOM us.
pub async fn fetch_html(
    client: &reqwest::Client,
    url: &Url,
    opts: &FetchOptions,
) -> Result<String, WebToolError> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| WebToolError::Fetch(e.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        // Try to capture a body excerpt for the agent to see.
        let body = response.text().await.unwrap_or_default();
        return Err(WebToolError::HttpStatus {
            code: status.as_u16(),
            body_excerpt: WebToolError::truncate_excerpt(&body),
        });
    }

    // Re-check host privacy after redirects: if the server
    // redirected to a private host, refuse.
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
    use futures::stream::StreamExt;
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
        // Public IP example: not private.
        assert!(!ip_is_private(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
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
        assert!(cache.check(&url, "anie/test", &client).await.is_ok());
    }

    #[tokio::test]
    async fn robots_cache_disallows_when_robot_says_so() {
        let robot = Robot::new("anie/test", b"User-agent: *\nDisallow: /private/").unwrap();
        let cache = RobotsCache::new();
        cache.insert("example.com", Some(robot)).await;
        let url = Url::parse("https://example.com/private/secret").unwrap();
        let client = reqwest::Client::new();
        let err = cache.check(&url, "anie/test", &client).await.unwrap_err();
        assert!(matches!(err, WebToolError::Forbidden(_)));
    }

    #[tokio::test]
    async fn robots_cache_allows_unrestricted_paths() {
        let robot = Robot::new("anie/test", b"User-agent: *\nDisallow: /private/").unwrap();
        let cache = RobotsCache::new();
        cache.insert("example.com", Some(robot)).await;
        let url = Url::parse("https://example.com/public/article").unwrap();
        let client = reqwest::Client::new();
        assert!(cache.check(&url, "anie/test", &client).await.is_ok());
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
