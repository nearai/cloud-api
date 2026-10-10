use crate::CompletionError;

/// Observed Chutes `/e2e/invoke` 403 detail. Keep this match narrow so ordinary
/// authorization failures are not retried. See https://github.com/nearai/cloud-api/issues/1221.
pub(super) const NONCE_REJECTED_DETAIL: &str = "Invalid, expired, or already-used nonce";

pub(super) fn retryable_provider_unavailable(ctx: &str, reason: &str) -> CompletionError {
    CompletionError::HttpError {
        status_code: 503,
        message: format!("{ctx}: Chutes temporarily unavailable ({reason})"),
        is_external: true,
    }
}

pub(super) fn stale_invoke_target(ctx: &str, status: u16, body: &str) -> bool {
    if !ctx.contains("/e2e/invoke") {
        return false;
    }
    // Chutes rejects an unusable nonce before inference starts. Do not retry
    // other 403s, which can indicate invalid credentials or missing access.
    if status == 403 {
        return serde_json::from_str::<serde_json::Value>(body).is_ok_and(|value| {
            value.get("detail").and_then(serde_json::Value::as_str) == Some(NONCE_REJECTED_DETAIL)
        });
    }
    if status != 400 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    let mentions_target = lower.contains("nonce") || lower.contains("instance");
    let stale = lower.contains("expired")
        || lower.contains("stale")
        || lower.contains("already used")
        || lower.contains("consumed")
        || lower.contains("not found")
        || lower.contains("invalid");
    mentions_target && stale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_invoke_target_requires_invoke_stage_and_stale_target_body() {
        assert!(stale_invoke_target(
            "Chutes /e2e/invoke",
            400,
            "nonce token expired for selected instance",
        ));
        assert!(!stale_invoke_target(
            "fetch evidence",
            400,
            "nonce token expired for selected instance",
        ));
        assert!(!stale_invoke_target(
            "Chutes /e2e/invoke",
            400,
            "malformed encrypted payload",
        ));
    }

    #[test]
    fn invoke_403_is_retryable_only_for_the_known_nonce_rejection() {
        // Keep the observed wire response independent of the matching constant.
        let nonce_error = r#"{"detail":"Invalid, expired, or already-used nonce"}"#;
        for ctx in ["Chutes /e2e/invoke", "Chutes /e2e/invoke (stream)"] {
            assert!(stale_invoke_target(ctx, 403, nonce_error));
            for body in [
                r#"{"detail":"Invalid API key"}"#,
                r#"{"detail":"Access to this instance is not allowed"}"#,
                r#"{"detail":"Invalid nonce signature"}"#,
                "Forbidden",
            ] {
                assert!(!stale_invoke_target(ctx, 403, body));
            }
            assert!(!stale_invoke_target(ctx, 401, nonce_error));
        }
        assert!(!stale_invoke_target("fetch evidence", 403, nonce_error));
        assert!(!stale_invoke_target("discover instances", 403, nonce_error));
    }

    #[test]
    fn retryable_provider_unavailable_is_http_503() {
        match retryable_provider_unavailable(
            "verify Chutes instance",
            "instance i1 not present in /evidence",
        ) {
            CompletionError::HttpError {
                status_code,
                message,
                is_external,
            } => {
                assert_eq!(status_code, 503);
                assert!(is_external);
                assert!(message.contains("verify Chutes instance"));
                assert!(message.contains("/evidence"));
            }
            other => panic!("retryable Chutes outage must map to HttpError, got {other:?}"),
        }
    }
}
