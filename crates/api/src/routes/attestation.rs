use crate::{models::ErrorResponse, ohttp_gateway::OhttpAttestation, routes::api::AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use inference_providers::ProviderTier;
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
    validate_signing_algo(params.signing_algo.as_deref())?;

    let signing_algo = params.signing_algo;
    let result = app_state
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
    State(app_state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, (StatusCode, ResponseJson<ErrorResponse>)> {
    // Surface alias resolution before fetching the report (issue #573):
    // a client asking for model X must learn — or, with x-no-aliasing,
    // refuse to learn — that the attestation (and the TD signing key it
    // binds) belongs to a different canonical model.
    let mut alias_resolved: Option<(String, String)> = None;
    if let Some(requested) = &params.model {
        match app_state
            .models_service
            .resolve_and_get_model(requested)
            .await
        {
            Ok(m) if &m.model_name != requested => {
                if crate::routes::common::no_aliasing_requested(&headers) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::with_param(
                            format!(
                                "Model '{requested}' is an alias of '{}' and the request set \
                                 {}. Use the canonical model name '{}'.",
                                m.model_name,
                                crate::routes::common::HEADER_NO_ALIASING,
                                m.model_name
                            ),
                            "invalid_request_error".to_string(),
                            "model".to_string(),
                        )),
                    ));
                }
                alias_resolved = Some((requested.clone(), m.model_name));
            }
            Ok(_) => {}
            // Unknown model: fall through — the attestation service
            // produces its own error for unknown models, and strict mode
            // only guards the alias-substitution case.
            Err(services::models::ModelsError::NotFound(_)) => {}
            Err(_) => {
                // Strict mode must fail closed: if the catalog can't be
                // consulted we can't guarantee no alias was applied —
                // and an E2EE client could bind a payload to the wrong
                // model TD's signing key.
                if crate::routes::common::no_aliasing_requested(&headers) {
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        ResponseJson(ErrorResponse::new(
                            "Failed to resolve model for x-no-aliasing check".to_string(),
                            "internal_server_error".to_string(),
                        )),
                    ));
                }
            }
        }
    }

    // Parse ?provider= into a ProviderTier filter.
    // Accepted values: "near" → Near, "chutes" → Attested3p.
    // Unknown values are rejected with 400 so callers notice typos immediately.
    let provider_filter = match params.provider.as_deref().map(str::to_ascii_lowercase).as_deref() {
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

    let report = app_state
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
    if let Some(ohttp) = &app_state.ohttp_attestation {
        response.ohttp_key_config = Some(ohttp.key_config.clone());
        response.ohttp_attestation = Some(ohttp.clone());
    }
    let mut http_response = axum::response::IntoResponse::into_response(ResponseJson(response));
    if let Some((requested, canonical)) = alias_resolved {
        if let Ok(value) = axum::http::HeaderValue::from_str(&format!("{requested} -> {canonical}"))
        {
            http_response.headers_mut().insert(
                axum::http::HeaderName::from_static(
                    crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED,
                ),
                value,
            );
            // Append to (rather than replace) any expose list set upstream.
            let expose_name = axum::http::HeaderName::from_static("access-control-expose-headers");
            let exposed = match http_response
                .headers()
                .get(&expose_name)
                .and_then(|v| v.to_str().ok())
            {
                Some(existing) => format!(
                    "{existing}, {}",
                    crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED
                ),
                None => crate::routes::common::HEADER_MODEL_ALIAS_RESOLVED.to_string(),
            };
            if let Ok(exposed) = axum::http::HeaderValue::from_str(&exposed) {
                http_response.headers_mut().insert(expose_name, exposed);
            }
        }
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

fn validate_signing_algo(
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

fn signature_error_response(error: AttestationError) -> (StatusCode, ResponseJson<ErrorResponse>) {
    let message = error.to_string();
    match &error {
        AttestationError::SignatureNotFound(_) => {
            error_response(StatusCode::NOT_FOUND, message, "not_found_error", None)
        }
        AttestationError::InvalidParameter(detail) => error_response(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            invalid_parameter_name(detail),
        ),
        AttestationError::ClientError(_) => error_response(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            None,
        ),
        AttestationError::ProviderError(_)
        | AttestationError::RepositoryError(_)
        | AttestationError::InternalError(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            message,
            "internal_server_error",
            None,
        ),
    }
}

fn attestation_report_error_response(
    error: AttestationError,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
    let message = error.to_string();
    match &error {
        AttestationError::InvalidParameter(detail) => error_response(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            invalid_parameter_name(detail),
        ),
        AttestationError::ClientError(_) => error_response(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            None,
        ),
        AttestationError::ProviderError(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            message,
            "provider_error",
            None,
        ),
        AttestationError::SignatureNotFound(_) => {
            error_response(StatusCode::NOT_FOUND, message, "not_found_error", None)
        }
        AttestationError::RepositoryError(_) | AttestationError::InternalError(_) => {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                message,
                "internal_server_error",
                None,
            )
        }
    }
}

fn internal_error_response(message: String) -> (StatusCode, ResponseJson<ErrorResponse>) {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        message,
        "internal_server_error",
        None,
    )
}

fn error_response(
    status: StatusCode,
    message: String,
    error_type: &str,
    param: Option<&str>,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
    let body = match param {
        Some(param) => {
            ErrorResponse::with_param(message, error_type.to_string(), param.to_string())
        }
        None => ErrorResponse::new(message, error_type.to_string()),
    };

    (status, ResponseJson(body))
}

fn invalid_parameter_name(message: &str) -> Option<&'static str> {
    let message = message.to_ascii_lowercase();
    if message.contains("nonce") {
        Some("nonce")
    } else if message.contains("signing algorithm") || message.contains("signing_algo") {
        Some("signing_algo")
    } else if message.contains("signing address") {
        Some("signing_address")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
