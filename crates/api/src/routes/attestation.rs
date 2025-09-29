use crate::routes::api::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for signature endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema, IntoParams)]
pub struct SignatureQuery {
    pub model: Option<String>,
    pub signing_algo: Option<String>,
}

/// Query parameters for attestation report
#[derive(Debug, Serialize, Deserialize, ToSchema, IntoParams)]
pub struct AttestationQuery {
    pub model: Option<String>,
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

/// Evidence item in NVIDIA payload
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Evidence {
    pub certificate: String,
}

/// NVIDIA attestation payload
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NvidiaPayload {
    pub nonce: String,
    pub evidence_list: Vec<Evidence>,
}

/// Individual attestation entry
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct Attestation {
    pub signing_address: String,
    pub intel_quote: String,
    pub nvidia_payload: String, // Stored as JSON string
}

/// Response for attestation report endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AttestationResponse {
    pub signing_address: String,
    pub intel_quote: String,
    pub nvidia_payload: String, // Stored as JSON string
    pub all_attestations: Vec<Attestation>,
}

impl From<inference_providers::AttestationReport> for AttestationResponse {
    fn from(report: inference_providers::AttestationReport) -> Self {
        Self {
            signing_address: report.signing_address,
            intel_quote: report.intel_quote,
            nvidia_payload: report.nvidia_payload,
            all_attestations: report
                .all_attestations
                .into_iter()
                .map(Attestation::from)
                .collect(),
        }
    }
}

impl From<inference_providers::AttestationReport> for Attestation {
    fn from(report: inference_providers::AttestationReport) -> Self {
        Self {
            signing_address: report.signing_address,
            intel_quote: report.intel_quote,
            nvidia_payload: report.nvidia_payload,
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/signature/{chat_id}",
    params(
        ("chat_id" = String, Path, description = "Chat completion ID"),
        SignatureQuery
    ),
    responses(
        (status = 200, description = "Signature retrieved successfully", body = SignatureResponse),
        (status = 404, description = "Signature not found"),
        (status = 400, description = "Invalid parameters")
    ),
    security(
        ("bearer_token" = [])
    )
)]
pub async fn get_signature(
    Path(chat_id): Path<String>,
    Query(_params): Query<SignatureQuery>,
    State(app_state): State<AppState>,
) -> Result<Json<SignatureResponse>, (StatusCode, Json<serde_json::Value>)> {
    let signature = app_state
        .attestation_service
        .get_chat_signature(chat_id.as_str())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let signature = signature.into();
    Ok(Json(signature))
}

#[utoipa::path(
    get,
    path = "/v1/attestation/report",
    params(
        AttestationQuery
    ),
    responses(
        (status = 200, description = "Attestation report retrieved successfully", body = AttestationResponse),
        (status = 503, description = "Attestation service unavailable")
    ),
    security(
        ("bearer_token" = [])
    )
)]
pub async fn get_attestation_report(
    Query(params): Query<AttestationQuery>,
    State(app_state): State<AppState>,
) -> Result<Json<AttestationResponse>, (StatusCode, Json<serde_json::Value>)> {
    let signing_algo = params.model.as_deref(); // Using model param as signing_algo for compatibility

    let report = app_state
        .attestation_service
        .get_attestation_report(signing_algo)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let response = report.into();
    Ok(Json(response))
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VerifyRequest {
    pub request_hash: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VerifyResponse {
    pub valid: bool,
    pub chat_id: String,
    pub response_hash: String,
    pub signature: Option<SignatureResponse>,
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/v1/verify/{chat_id}",
    params(
        ("chat_id" = String, Path, description = "Chat completion ID to verify")
    ),
    request_body = VerifyRequest,
    responses(
        (status = 200, description = "Verification completed", body = VerifyResponse)
    ),
    security(
        ("bearer_token" = [])
    )
)]
pub async fn verify_attestation(
    Path(_chat_id): Path<String>,
    State(_app_state): State<AppState>,
    Json(_req): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, (StatusCode, Json<serde_json::Value>)> {
    unimplemented!()
}

// Re-export types for use in router configuration
pub use AttestationResponse as AttestationResponseType;
pub use SignatureResponse as SignatureResponseType;
