use crate::{ohttp_gateway::OhttpAttestation, routes::api::AppState};
use axum::{routing::get, Router};
use services::models::ModelsServiceTrait;
use std::sync::Arc;

mod alias;
mod errors;
pub(crate) mod ita_token;
mod ita_token_models;
pub(crate) mod report;
pub(crate) mod signature;

pub use ita_token::get_ita_token;
pub use ita_token_models::{
    ItaModelAliasResolved, ItaModelTokenItem, ItaTokenItem, ItaTokenQuery, ItaTokenResponse,
};
pub use report::{
    get_attestation_report, AttestationQuery, AttestationResponse, DstackCpuQuote, Evidence,
    NvidiaPayload, QuoteResponse, VerifyRequest, VpcInfo,
};
pub use signature::{
    get_signature, SignatureQuery, SignatureResponse, SignatureUnavailableResponse,
};

#[derive(Clone)]
pub struct AttestationRouteState {
    pub attestation_service: Arc<dyn services::attestation::ports::AttestationServiceTrait>,
    pub models_service: Arc<dyn ModelsServiceTrait>,
    pub ohttp_attestation: Option<OhttpAttestation>,
}

impl From<AppState> for AttestationRouteState {
    fn from(app_state: AppState) -> Self {
        Self {
            attestation_service: app_state.attestation_service,
            models_service: app_state.models_service,
            ohttp_attestation: app_state.ohttp_attestation,
        }
    }
}

pub fn build_public_attestation_routes(state: AttestationRouteState) -> Router {
    Router::new()
        .route("/attestation/report", get(get_attestation_report))
        .route("/attestation/ita-token", get(get_ita_token))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::{
        errors::{attestation_report_error_response, signature_error_response},
        signature::validate_signing_algo,
    };
    use axum::{http::StatusCode, response::Json as ResponseJson};
    use services::attestation::AttestationError;

    mod ita_token_route_test_support;
    mod ita_token_route_tests;

    #[test]
    fn signature_not_found_uses_standard_not_found_envelope() {
        let (status, ResponseJson(body)) = signature_error_response(
            AttestationError::SignatureNotFound("chatcmpl-test:ecdsa".to_string()),
        );

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            body.error.message,
            "Signature not found: chatcmpl-test:ecdsa"
        );
        assert_eq!(body.error.r#type, "not_found_error");
        assert_eq!(body.error.param, None);
        assert_eq!(body.error.code, None);
    }

    #[test]
    fn invalid_report_nonce_is_400_with_param() {
        let (status, ResponseJson(body)) =
            attestation_report_error_response(AttestationError::InvalidParameter(
                "Nonce must be exactly 32 bytes, got 1 bytes".to_string(),
            ));

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.error.message,
            "Invalid parameter: Nonce must be exactly 32 bytes, got 1 bytes"
        );
        assert_eq!(body.error.r#type, "invalid_request_error");
        assert_eq!(body.error.param.as_deref(), Some("nonce"));
        assert_eq!(body.error.code, None);
    }

    #[test]
    fn invalid_signature_algorithm_is_rejected_before_lookup() {
        let (status, ResponseJson(body)) =
            validate_signing_algo(Some("rsa")).expect_err("rsa must be rejected");

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.error.message,
            "Invalid signing algorithm: rsa, must be 'ecdsa' or 'ed25519'"
        );
        assert_eq!(body.error.r#type, "invalid_request_error");
        assert_eq!(body.error.param.as_deref(), Some("signing_algo"));
        assert_eq!(body.error.code, None);
    }

    #[test]
    fn supported_signature_algorithms_are_accepted() {
        assert!(validate_signing_algo(None).is_ok());
        assert!(validate_signing_algo(Some("ecdsa")).is_ok());
        assert!(validate_signing_algo(Some("ed25519")).is_ok());
        assert!(validate_signing_algo(Some("ECDSA")).is_ok());
        assert!(validate_signing_algo(Some("ED25519")).is_ok());
    }
}
