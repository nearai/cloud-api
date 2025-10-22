use crate::routes::api::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
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
    pub nonce: Option<String>,
    pub signing_address: Option<String>,
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

/// Response for attestation report endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DstackCpuQuote {
    pub quote: String,
    pub event_log: String,
    pub report_data: String,
    pub request_nonce: String,
    pub info: serde_json::Value,
}

impl From<services::attestation::models::DstackCpuQuote> for DstackCpuQuote {
    fn from(quote: services::attestation::models::DstackCpuQuote) -> Self {
        Self {
            quote: quote.quote,
            event_log: quote.event_log,
            report_data: quote.report_data,
            request_nonce: quote.request_nonce,
            info: quote.info,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AttestationResponse {
    pub gateway_attestation: DstackCpuQuote,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub all_attestations: Vec<serde_json::Map<String, serde_json::Value>>,
    /// Deprecated: use `all_attestations` instead
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_attestations: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl From<services::attestation::models::AttestationReport> for AttestationResponse {
    fn from(report: services::attestation::models::AttestationReport) -> Self {
        Self {
            gateway_attestation: report.gateway_attestation.into(),
            all_attestations: report.all_attestations.clone(),
            model_attestations: report.all_attestations,
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
        (status = 400, description = "Invalid nonce format"),
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
    let report = app_state
        .attestation_service
        .get_attestation_report(
            params.model,
            params.signing_algo,
            params.nonce,
            params.signing_address,
        )
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

impl From<services::attestation::models::DstackCpuQuote> for QuoteResponse {
    fn from(response: services::attestation::models::DstackCpuQuote) -> Self {
        Self {
            quote: response.quote,
            event_log: response.event_log,
        }
    }
}

/// Error response
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}
