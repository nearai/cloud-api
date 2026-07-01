use super::{errors::*, AttestationRouteState};
use crate::models::ErrorResponse;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::{Deserialize, Serialize};
use services::attestation::SignatureLookupResult;
use utoipa::{IntoParams, ToSchema};

/// Query parameters for signature endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema, IntoParams)]
pub struct SignatureQuery {
    pub model: Option<String>,
    pub signing_algo: Option<String>,
}

/// Response for signature endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SignatureResponse {
    pub text: String,
    pub signature: String,
    pub signing_address: String,
    pub signing_algo: String,
}

impl From<services::attestation::ChatSignature> for SignatureResponse {
    fn from(sig: services::attestation::ChatSignature) -> Self {
        Self {
            text: sig.text,
            signature: sig.signature,
            signing_address: sig.signing_address,
            signing_algo: sig.signing_algo,
        }
    }
}

/// Response when signature is unavailable (e.g., due to client disconnect)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SignatureUnavailableResponse {
    pub error_code: String,
    pub message: String,
}

/// Get completion signature
///
/// Get cryptographic signature for a chat completion for verification.
/// Returns signature data on success, or an unavailable response if the stream was disconnected.
#[utoipa::path(
    get,
    path = "/v1/signature/{chat_id}",
    params(
        ("chat_id" = String, Path, description = "Chat completion ID"),
        SignatureQuery
    ),
    responses(
        (status = 200, description = "Signature retrieved or unavailable due to disconnect", body = SignatureResponse),
        (status = 404, description = "Signature not found", body = ErrorResponse),
        (status = 400, description = "Invalid parameters", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    ),
    tag = "Attestation"
)]
pub async fn get_signature(
    Path(chat_id): Path<String>,
    Query(params): Query<SignatureQuery>,
    State(state): State<AttestationRouteState>,
) -> Result<ResponseJson<serde_json::Value>, (StatusCode, ResponseJson<ErrorResponse>)> {
    validate_signing_algo(params.signing_algo.as_deref())?;

    let signing_algo = params.signing_algo;
    let result = state
        .attestation_service
        .get_chat_signature(chat_id.as_str(), signing_algo)
        .await
        .map_err(signature_error_response)?;

    // Handle both Found and Unavailable results
    match result {
        SignatureLookupResult::Found(signature) => {
            let response: SignatureResponse = signature.into();
            serde_json::to_value(response)
                .map(ResponseJson)
                .map_err(|e| {
                    internal_error_response(format!("Failed to serialize signature response: {e}"))
                })
        }
        SignatureLookupResult::Unavailable {
            error_code,
            message,
        } => {
            let response = SignatureUnavailableResponse {
                error_code,
                message,
            };
            serde_json::to_value(response)
                .map(ResponseJson)
                .map_err(|e| {
                    internal_error_response(format!(
                        "Failed to serialize unavailable response: {e}"
                    ))
                })
        }
    }
}

pub(super) fn validate_signing_algo(
    signing_algo: Option<&str>,
) -> Result<(), (StatusCode, ResponseJson<ErrorResponse>)> {
    match signing_algo.map(str::to_ascii_lowercase).as_deref() {
        None | Some("ecdsa") | Some("ed25519") => Ok(()),
        Some(signing_algo) => Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid signing algorithm: {signing_algo}, must be 'ecdsa' or 'ed25519'"),
            "invalid_request_error",
            Some("signing_algo"),
        )),
    }
}
