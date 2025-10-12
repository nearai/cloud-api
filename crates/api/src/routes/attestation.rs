use crate::routes::api::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use serde::{Deserialize, Serialize};
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

#[utoipa::path(
    get,
    path = "/signature/{chat_id}",
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
        ("api_key" = [])
    )
)]
pub async fn get_signature(
    Path(chat_id): Path<String>,
    Query(_params): Query<SignatureQuery>,
    State(app_state): State<AppState>,
) -> Result<ResponseJson<SignatureResponse>, (StatusCode, ResponseJson<serde_json::Value>)> {
    let signature = app_state
        .attestation_service
        .get_chat_signature(chat_id.as_str())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let signature = signature.into();
    Ok(ResponseJson(signature))
}

/// Query parameters for attestation report
#[derive(Debug, Serialize, Deserialize, ToSchema, IntoParams)]
pub struct AttestationQuery {
    pub model: Option<String>,
    pub signing_algo: Option<String>,
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

impl From<inference_providers::AttestationReport> for Attestation {
    fn from(report: inference_providers::AttestationReport) -> Self {
        Self {
            signing_address: report.signing_address,
            intel_quote: report.intel_quote,
            nvidia_payload: report.nvidia_payload,
        }
    }
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

#[utoipa::path(
    get,
    path = "/attestation/report",
    params(
        AttestationQuery
    ),
    responses(
        (status = 200, description = "Attestation report retrieved successfully", body = AttestationResponse),
        (status = 503, description = "Attestation service unavailable")
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn get_attestation_report(
    Query(params): Query<AttestationQuery>,
    State(app_state): State<AppState>,
) -> Result<ResponseJson<AttestationResponse>, (StatusCode, ResponseJson<serde_json::Value>)> {
    let model = if let Some(model) = params.model {
        model
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(serde_json::json!({ "error": "model is required" })),
        ));
    };

    let report = app_state
        .attestation_service
        .get_attestation_report(model, params.signing_algo)
        .await
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ResponseJson(serde_json::json!({ "error": e.to_string() })),
            )
        })?;

    let response = report.into();
    Ok(ResponseJson(response))
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VerifyRequest {
    pub request_hash: Option<String>,
}

/// Quote response containing gateway quote and allowlist
#[derive(Debug, Serialize, ToSchema)]
pub struct QuoteResponse {
    /// The attestation quote in hexadecimal format
    pub quote: String,
    /// The event log associated with the quote
    pub event_log: String,
}

impl From<services::attestation::models::GetQuoteResponse> for QuoteResponse {
    fn from(response: services::attestation::models::GetQuoteResponse) -> Self {
        Self {
            quote: response.quote,
            event_log: response.event_log,
        }
    }
}

/// Get TDX quote
///
/// Returns a TDX quote for testing purposes.
#[utoipa::path(
    get,
    path = "/attestation/quote", 
    tag = "Attestation",
    responses(
        (status = 200, description = "TDX quote", body = QuoteResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 501, description = "Not implemented", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn quote(
    State(app_state): State<AppState>,
    Extension(_api_key): Extension<services::auth::ApiKey>,
) -> Result<ResponseJson<QuoteResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let quote = app_state
        .attestation_service
        .get_quote()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?
        .into();
    Ok(ResponseJson(quote))
}

/// Error response
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}
