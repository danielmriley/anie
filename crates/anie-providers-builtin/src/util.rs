/// Parse an HTTP retry-after header into milliseconds.
pub fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds * 1_000)
}

/// Classify a non-success HTTP response into a structured provider error.
pub fn classify_http_error(
    status: reqwest::StatusCode,
    body: &str,
    retry_after_ms: Option<u64>,
) -> anie_provider::ProviderError {
    match status.as_u16() {
        401 | 403 => anie_provider::ProviderError::Auth(body.to_string()),
        429 | 529 => anie_provider::ProviderError::RateLimited { retry_after_ms },
        400 => {
            // Plan 06 PR-G: lowercase the body once per
            // classification, not twice. Keeps both keyword
            // checks pointing at the same allocation.
            let body_lower = body.to_ascii_lowercase();
            if body_lower.contains("context") || body_lower.contains("token") {
                anie_provider::ProviderError::ContextOverflow(body.to_string())
            } else {
                anie_provider::ProviderError::Http {
                    status: status.as_u16(),
                    body: body.to_string(),
                }
            }
        }
        _ => anie_provider::ProviderError::Http {
            status: status.as_u16(),
            body: body.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_auth_errors() {
        let error = classify_http_error(reqwest::StatusCode::UNAUTHORIZED, "nope", None);
        assert!(matches!(error, anie_provider::ProviderError::Auth(message) if message == "nope"));
    }

    #[test]
    fn classifies_rate_limits() {
        let error = classify_http_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            Some(2_000),
        );
        assert!(matches!(
            error,
            anie_provider::ProviderError::RateLimited {
                retry_after_ms: Some(2_000)
            }
        ));
    }

    #[test]
    fn classifies_context_overflow() {
        let error = classify_http_error(
            reqwest::StatusCode::BAD_REQUEST,
            "context window exceeded",
            None,
        );
        assert!(
            matches!(error, anie_provider::ProviderError::ContextOverflow(message) if message.contains("context"))
        );
    }
}
