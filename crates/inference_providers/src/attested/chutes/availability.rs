use crate::CompletionError;

pub(super) fn retryable_provider_unavailable(ctx: &str, reason: &str) -> CompletionError {
    CompletionError::HttpError {
        status_code: 503,
        message: format!("{ctx}: Chutes temporarily unavailable ({reason})"),
        is_external: true,
    }
}

pub(super) fn stale_invoke_target(ctx: &str, body: &str) -> bool {
    if !ctx.contains("/e2e/invoke") {
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
            "nonce token expired for selected instance",
        ));
        assert!(!stale_invoke_target(
            "fetch evidence",
            "nonce token expired for selected instance",
        ));
        assert!(!stale_invoke_target(
            "Chutes /e2e/invoke",
            "malformed encrypted payload",
        ));
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
