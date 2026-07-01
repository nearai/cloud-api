use super::{
    alias::{attach_alias_header, resolve_attestation_alias},
    errors::{attestation_report_error_response, error_response},
    AttestationRouteState,
};
use crate::{models::ErrorResponse, ohttp_gateway::OhttpAttestation};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Json as ResponseJson,
};
use inference_providers::ProviderTier;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for attestation report
#[derive(Debug, Serialize, Deserialize, ToSchema, IntoParams)]
pub struct AttestationQuery {
    pub model: Option<String>,
    pub signing_algo: Option<String>,
    pub nonce: Option<String>,
    pub signing_address: Option<String>,
    /// Include TLS certificate fingerprint in the report data.
    /// Defaults to false; when true, report_data[..32] = SHA256(signing_address || cert_fingerprint).
    pub include_tls_fingerprint: Option<bool>,
    /// Restrict the report to a specific serving tier.
    /// Accepted values: `near` (NEAR AI's own TEE fleet) or `chutes` (attested Chutes fallback).
    /// When omitted, the first successfully responding provider is used.
    pub provider: Option<String>,
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
    /// SHA-256 hash of the TLS certificate's SPKI, if requested via include_tls_fingerprint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_fingerprint: Option<String>,
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
            tls_cert_fingerprint: quote.tls_cert_fingerprint,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AttestationResponse {
    pub gateway_attestation: DstackCpuQuote,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_attestations: Vec<serde_json::Map<String, serde_json::Value>>,
    /// TLS certificate file (PEM) from TLS_CERT_PATH; report_data binds via SHA256 of these exact bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_certificate: Option<String>,
    /// Hex-encoded OHTTP key configuration (RFC 9458). Present only when OHTTP_ENABLED=true.
    /// Legacy flat field; mirrors `ohttp_attestation.key_config` when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ohttp_key_config: Option<String>,
    /// OHTTP key attestation payload. Includes an Ed25519 signature over the decoded
    /// `key_config` bytes so clients can verify the HPKE key is bound to the TEE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ohttp_attestation: Option<OhttpAttestation>,
}

impl From<services::attestation::models::AttestationReport> for AttestationResponse {
    fn from(report: services::attestation::models::AttestationReport) -> Self {
        Self {
            gateway_attestation: report.gateway_attestation.into(),
            model_attestations: report.model_attestations,
            tls_certificate: report.tls_certificate,
            ohttp_key_config: None,
            ohttp_attestation: None,
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
    State(state): State<AttestationRouteState>,
    headers: HeaderMap,
) -> Result<axum::response::Response, (StatusCode, ResponseJson<ErrorResponse>)> {
    let alias_resolved =
        resolve_attestation_alias(params.model.as_deref(), &state.models_service, &headers).await?;
    let provider_filter = match params
        .provider
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None => None,
        Some("near") => Some(ProviderTier::Near),
        Some("chutes") => Some(ProviderTier::Attested3p),
        Some(unknown) => {
            return Err(error_response(
                StatusCode::BAD_REQUEST,
                format!("Unknown provider '{unknown}'. Accepted values: 'near', 'chutes'."),
                "invalid_request_error",
                Some("provider"),
            ));
        }
    };

    let report = state
        .attestation_service
        .get_attestation_report(
            params.model,
            params.signing_algo,
            params.nonce,
            params.signing_address,
            params.include_tls_fingerprint.unwrap_or(false),
            provider_filter,
        )
        .await
        .map_err(attestation_report_error_response)?;

    let mut response: AttestationResponse = report.into();
    if let Some(ohttp) = &state.ohttp_attestation {
        response.ohttp_key_config = Some(ohttp.key_config.clone());
        response.ohttp_attestation = Some(ohttp.clone());
    }
    let mut http_response = axum::response::IntoResponse::into_response(ResponseJson(response));
    if let Some((requested, canonical)) = alias_resolved {
        attach_alias_header(&mut http_response, &requested, &canonical);
    }
    Ok(http_response)
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
