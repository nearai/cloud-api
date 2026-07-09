use reqwest::StatusCode;

use crate::attestation::ita::ItaVerifierNonceDecodeError;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ItaClientError {
    #[error("ITA API key is not configured")]
    MissingCredentials,
    #[error("invalid ITA client configuration: {reason}")]
    InvalidConfig { reason: &'static str },
    #[error("invalid ITA request id header")]
    InvalidRequestId,
    #[error("failed to build ITA HTTP client")]
    ClientBuild {
        #[source]
        source: reqwest::Error,
    },
    #[error("ITA request timed out")]
    Timeout,
    #[error("ITA transport error")]
    Transport {
        retryable: bool,
        #[source]
        source: reqwest::Error,
    },
    #[error("ITA rate limited")]
    RateLimited { retry_after: Option<String> },
    #[error("ITA returned non-retryable status {status}")]
    NonRetryableStatus { status: StatusCode },
    #[error("ITA transient status {status} remained after retries")]
    TransientStatus { status: StatusCode },
    #[error("ITA upstream response is invalid: {reason}")]
    UpstreamResponse { reason: &'static str },
    #[error("ITA verifier nonce is invalid")]
    InvalidVerifierNonce {
        #[source]
        source: ItaVerifierNonceDecodeError,
    },
}

impl ItaClientError {
    pub(super) fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout | Self::TransientStatus { .. } => true,
            Self::Transport { retryable, .. } => *retryable,
            // 429 is deliberately NOT retried: the fixed backoff is far shorter
            // than any realistic Retry-After window, so retrying just burns ITA
            // quota while the caller waits. Surface it immediately — Retry-After
            // is preserved on the typed error for API-layer propagation.
            Self::RateLimited { .. } => false,
            Self::MissingCredentials
            | Self::InvalidConfig { .. }
            | Self::InvalidRequestId
            | Self::ClientBuild { .. }
            | Self::NonRetryableStatus { .. }
            | Self::UpstreamResponse { .. }
            | Self::InvalidVerifierNonce { .. } => false,
        }
    }
}
