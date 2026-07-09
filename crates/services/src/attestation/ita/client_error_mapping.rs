use crate::attestation::{ita::ItaClientError, AttestationError};

pub(in crate::attestation::ita) fn map_ita_client_error(error: ItaClientError) -> AttestationError {
    match error {
        ItaClientError::MissingCredentials | ItaClientError::InvalidConfig { .. } => {
            AttestationError::ItaUnavailable {
                reason: "ITA client is not configured".to_string(),
            }
        }
        ItaClientError::RateLimited { retry_after } => {
            AttestationError::ItaRateLimited { retry_after }
        }
        ItaClientError::Timeout => AttestationError::ItaTimeout,
        ItaClientError::NonRetryableStatus { status } if status.as_u16() == 400 => {
            AttestationError::ItaInvalidEvidence {
                reason: format!("ITA rejected evidence with status {status}"),
            }
        }
        ItaClientError::NonRetryableStatus { status } => AttestationError::ItaBadUpstream {
            reason: format!("ITA returned status {status}"),
        },
        ItaClientError::TransientStatus { status } => AttestationError::ItaBadUpstream {
            reason: format!("ITA transient status {status} remained after retries"),
        },
        ItaClientError::Transport { .. }
        | ItaClientError::ClientBuild { .. }
        | ItaClientError::InvalidRequestId
        | ItaClientError::UpstreamResponse { .. }
        | ItaClientError::InvalidVerifierNonce { .. } => AttestationError::ItaBadUpstream {
            reason: error.to_string(),
        },
    }
}

pub(in crate::attestation) fn ita_client_error_class(error: &ItaClientError) -> &'static str {
    match error {
        ItaClientError::MissingCredentials => "missing_credentials",
        ItaClientError::InvalidConfig { .. } => "invalid_config",
        ItaClientError::InvalidRequestId => "invalid_request_id",
        ItaClientError::ClientBuild { .. } => "client_build",
        ItaClientError::Timeout => "timeout",
        ItaClientError::Transport { .. } => "transport",
        ItaClientError::RateLimited { .. } => "rate_limited",
        ItaClientError::NonRetryableStatus { .. } => "non_retryable_status",
        ItaClientError::TransientStatus { .. } => "transient_status",
        ItaClientError::UpstreamResponse { .. } => "upstream_response",
        ItaClientError::InvalidVerifierNonce { .. } => "invalid_verifier_nonce",
    }
}
