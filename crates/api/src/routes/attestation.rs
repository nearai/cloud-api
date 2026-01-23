use crate::routes::api::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::{Deserialize, Serialize};
use services::attestation::{AttestationError, SignatureLookupResult};
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
    State(app_state): State<AppState>,
) -> Result<ResponseJson<serde_json::Value>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let signing_algo = params.signing_algo;
    let result = app_state
        .attestation_service
        .get_chat_signature(chat_id.as_str(), signing_algo)
        .await
        .map_err(|e| {
            let status_code = match e {
                AttestationError::SignatureNotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status_code,
                ResponseJson(ErrorResponse {
                    error: e.to_string(),
                }),
            )
        })?;

    // Handle both Found and Unavailable results
    match result {
        SignatureLookupResult::Found(signature) => {
            let response: SignatureResponse = signature.into();
            Ok(ResponseJson(serde_json::to_value(response).unwrap()))
        }
        SignatureLookupResult::Unavailable {
            error_code,
            message,
        } => Ok(ResponseJson(serde_json::json!({
            "error_code": error_code,
            "message": message
        }))),
    }
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

/// VPC information in attestation
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VpcInfo {
    /// VPC server app ID
    pub vpc_server_app_id: Option<String>,
    /// VPC hostname of this node
    pub vpc_hostname: Option<String>,
}

/// Response for attestation report endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DstackCpuQuote {
    /// The signing address used for the attestation
    pub signing_address: String,
    /// The signing algorithm used for the attestation (ecdsa or ed25519)
    pub signing_algo: String,
    /// The attestation quote in hexadecimal format
    pub intel_quote: String,
    /// The event log associated with the quote
    pub event_log: String,
    /// The report data that contains signing address and nonce
    pub report_data: String,
    /// The nonce used in the attestation request
    pub request_nonce: String,
    /// Application info from Dstack
    pub info: serde_json::Value,
    /// VPC information (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vpc: Option<VpcInfo>,
}

impl From<services::attestation::models::DstackCpuQuote> for DstackCpuQuote {
    fn from(quote: services::attestation::models::DstackCpuQuote) -> Self {
        Self {
            signing_address: quote.signing_address,
            signing_algo: quote.signing_algo,
            intel_quote: quote.intel_quote,
            event_log: quote.event_log,
            report_data: quote.report_data,
            request_nonce: quote.request_nonce,
            info: quote.info,
            vpc: quote.vpc.map(|v| VpcInfo {
                vpc_server_app_id: v.vpc_server_app_id,
                vpc_hostname: v.vpc_hostname,
            }),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AttestationResponse {
    pub gateway_attestation: DstackCpuQuote,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_attestations: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl From<services::attestation::models::AttestationReport> for AttestationResponse {
    fn from(report: services::attestation::models::AttestationReport) -> Self {
        Self {
            gateway_attestation: report.gateway_attestation.into(),
            model_attestations: report.model_attestations,
        }
    }
}

/// Get attestation report
///
/// Get hardware attestation report for TEE verification. Public endpoint.
#[utoipa::path(
    get,
    path = "/v1/attestation/report",
    params(
        AttestationQuery
    ),
    responses(
        (status = 200, description = "Attestation report retrieved", body = AttestationResponse),
        (status = 400, description = "Invalid nonce format", body = ErrorResponse),
        (status = 503, description = "Service unavailable", body = ErrorResponse)
    ),
    tag = "Attestation"
)]
pub async fn get_attestation_report(
    Query(params): Query<AttestationQuery>,
    State(app_state): State<AppState>,
) -> Result<ResponseJson<AttestationResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
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
                ResponseJson(ErrorResponse {
                    error: e.to_string(),
                }),
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
    pub intel_quote: String,
    /// The event log associated with the quote
    pub event_log: String,
}

impl From<services::attestation::models::DstackCpuQuote> for QuoteResponse {
    fn from(response: services::attestation::models::DstackCpuQuote) -> Self {
        Self {
            intel_quote: response.intel_quote,
            event_log: response.event_log,
        }
    }
}

/// Error response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}
