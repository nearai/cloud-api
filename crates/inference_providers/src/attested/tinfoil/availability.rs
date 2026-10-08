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

pub fn map_upstream_status(status: u16) -> UpstreamDisposition {
    match status {
        // Our key or billing: must not short-circuit past the other providers.
        401..=403 => UpstreamDisposition::Retryable503,
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
