//! Maps Tinfoil upstream outcomes onto the pool's retry semantics (spec §3.5).
//!
//! The pool retries any status >= 500 and 429 at the round level, and returns
//! every other 4xx immediately. Tinfoil is the backup, so anything that is not
//! the caller's own fault must look retryable.

use crate::CompletionError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamDisposition {
    /// Report 503 (retryable) so the pool moves on to the next provider.
    Retryable503,
    /// Pass 429 through (retryable with backoff).
    Passthrough429,
    /// The request itself is bad; return that 4xx.
    ReturnAs4xx(u16),
}

/// Upstream 401/402/403: our key or billing, not the caller's fault. Single
/// source of truth for both the status mapping and the auth-failure bookkeeping.
pub fn is_auth_status(status: u16) -> bool {
    matches!(status, 401..=403)
}

pub fn map_upstream_status(status: u16) -> UpstreamDisposition {
    match status {
        // Our key or billing: must not short-circuit past the other providers.
        s if is_auth_status(s) => UpstreamDisposition::Retryable503,
        429 => UpstreamDisposition::Passthrough429,
        // Not the caller's fault: the router has no such route (404), timed out
        // reading the request (408) or rejected an early-data replay (425).
        404 | 408 | 425 => UpstreamDisposition::Retryable503,
        400..=499 => UpstreamDisposition::ReturnAs4xx(status),
        // 5xx, and anything unexpected (redirects are never followed).
        _ => UpstreamDisposition::Retryable503,
    }
}

/// The backup is unavailable (not verified, pins miss, transport failure...).
pub fn unavailable(reason: &'static str) -> CompletionError {
    CompletionError::HttpError {
        status_code: 503,
        message: format!("Tinfoil temporarily unavailable ({reason})"),
        is_external: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_statuses_are_one_source_of_truth() {
        for s in [401u16, 402, 403] {
            assert!(is_auth_status(s));
            assert_eq!(map_upstream_status(s), UpstreamDisposition::Retryable503);
        }
        for s in [400u16, 404, 429, 500] {
            assert!(!is_auth_status(s));
        }
    }
}
