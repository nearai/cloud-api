use reqwest::StatusCode;

use super::map_ita_client_error;
use crate::attestation::{ita::ItaClientError, AttestationError};

#[test]
fn evidence_rejection_detail_reaches_the_client_facing_reason() {
    // Given: ITA rejects evidence with a diagnostic body.
    let error = map_ita_client_error(ItaClientError::NonRetryableStatus {
        status: StatusCode::BAD_REQUEST,
        detail: Some("Failed to verify GPU evidence".to_string()),
    });

    // Then: the 400 reason carries the upstream diagnostic for the caller.
    assert!(matches!(
        error,
        AttestationError::ItaInvalidEvidence { reason }
            if reason == "ITA rejected evidence with status 400 Bad Request: Failed to verify GPU evidence"
    ));
}

#[test]
fn non_evidence_status_detail_stays_out_of_the_client_facing_reason() {
    // Given: ITA returns an auth failure with account text in the body.
    let error = map_ita_client_error(ItaClientError::NonRetryableStatus {
        status: StatusCode::FORBIDDEN,
        detail: Some("tenant 1234 request abc".to_string()),
    });

    // Then: the surfaced reason is the bare status; the detail is log-only.
    assert!(matches!(
        error,
        AttestationError::ItaBadUpstream { reason } if reason == "ITA returned status 403 Forbidden"
    ));
}
