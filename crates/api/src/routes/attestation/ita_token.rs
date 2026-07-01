use super::{
    alias::{attach_alias_header, resolve_attestation_alias},
    errors::{error_response, error_tuple_into_response, ita_token_error_response},
    ita_token_models::{ItaTokenQuery, ItaTokenResponse},
    AttestationRouteState,
};
use crate::models::ErrorResponse;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json as ResponseJson, Response},
};
use services::attestation::ita::{
    ItaGatewaySigningAlg as ServiceItaGatewaySigningAlg, ItaTokenQuery as ServiceItaTokenQuery,
};

/// Get Intel Trust Authority attestation token
///
/// Get Intel Trust Authority signed attestation JWTs for the gateway and, when
/// requested, compatible model provider evidence. Public endpoint.
#[utoipa::path(
    get,
    path = "/v1/attestation/ita-token",
    params(
        ItaTokenQuery
    ),
    responses(
        (status = 200, description = "ITA attestation token retrieved", body = ItaTokenResponse),
        (status = 400, description = "Invalid parameters", body = ErrorResponse),
        (status = 429, description = "ITA rate limit exceeded", body = ErrorResponse),
        (status = 502, description = "Bad ITA upstream response", body = ErrorResponse),
        (status = 503, description = "ITA attestation unavailable", body = ErrorResponse),
        (status = 504, description = "ITA request timed out", body = ErrorResponse)
    ),
    security(()),
    tag = "Attestation"
)]
pub async fn get_ita_token(
    Query(params): Query<ItaTokenQuery>,
    State(state): State<AttestationRouteState>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let alias_resolved =
        resolve_attestation_alias(params.model.as_deref(), &state.models_service, &headers)
            .await
            .map_err(error_tuple_into_response)?;
    let query = params
        .into_service_query()
        .map_err(error_tuple_into_response)?;
    let response = state
        .attestation_service
        .get_ita_attestation_token(query)
        .await
        .map_err(ita_token_error_response)?;

    let mut http_response = ResponseJson(ItaTokenResponse::from(response)).into_response();
    if let Some((requested, canonical)) = alias_resolved {
        attach_alias_header(&mut http_response, &requested, &canonical);
    }
    Ok(http_response)
}

impl ItaTokenQuery {
    fn into_service_query(
        self,
    ) -> Result<ServiceItaTokenQuery, (StatusCode, ResponseJson<ErrorResponse>)> {
        validate_ita_nonce(self.nonce.as_deref())?;
        Ok(ServiceItaTokenQuery {
            model: self.model,
            nonce: self.nonce,
            signing_algo: parse_ita_gateway_signing_alg(self.signing_algo.as_deref())?,
            signing_address: self.signing_address,
            include_tls_fingerprint: parse_optional_bool(
                self.include_tls_fingerprint.as_deref(),
                "include_tls_fingerprint",
            )?,
            policy_ids: match self.policy_ids {
                Some(raw) => Some(config::ItaPolicyIds::parse_csv(&raw, "policy_ids").map_err(
                    |message| {
                        error_response(
                            StatusCode::BAD_REQUEST,
                            message,
                            "invalid_request_error",
                            Some("policy_ids"),
                        )
                    },
                )?),
                None => None,
            },
            policy_must_match: parse_optional_bool(
                self.policy_must_match.as_deref(),
                "policy_must_match",
            )?,
            token_signing_alg: match self.token_signing_alg {
                Some(raw) => Some(raw.parse::<config::ItaTokenSigningAlg>().map_err(
                    |message| {
                        error_response(
                            StatusCode::BAD_REQUEST,
                            message,
                            "invalid_request_error",
                            Some("token_signing_alg"),
                        )
                    },
                )?),
                None => None,
            },
        })
    }
}

fn validate_ita_nonce(
    nonce: Option<&str>,
) -> Result<(), (StatusCode, ResponseJson<ErrorResponse>)> {
    let Some(nonce) = nonce else {
        return Ok(());
    };
    match hex::decode(nonce) {
        Ok(bytes) if bytes.len() == 32 => Ok(()),
        Ok(bytes) => Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Nonce must be exactly 32 bytes, got {} bytes", bytes.len()),
            "invalid_request_error",
            Some("nonce"),
        )),
        Err(error) => Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid nonce format: {error}"),
            "invalid_request_error",
            Some("nonce"),
        )),
    }
}

fn parse_ita_gateway_signing_alg(
    raw: Option<&str>,
) -> Result<Option<ServiceItaGatewaySigningAlg>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match raw.map(str::to_ascii_lowercase).as_deref() {
        None => Ok(None),
        Some("ed25519") => Ok(Some(ServiceItaGatewaySigningAlg::Ed25519)),
        Some("ecdsa") => Ok(Some(ServiceItaGatewaySigningAlg::Ecdsa)),
        Some(signing_algo) => Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("Invalid signing algorithm: {signing_algo}, must be 'ecdsa' or 'ed25519'"),
            "invalid_request_error",
            Some("signing_algo"),
        )),
    }
}

fn parse_optional_bool(
    raw: Option<&str>,
    param: &'static str,
) -> Result<Option<bool>, (StatusCode, ResponseJson<ErrorResponse>)> {
    match raw.map(str::trim) {
        None => Ok(None),
        Some("true") | Some("1") => Ok(Some(true)),
        Some("false") | Some("0") => Ok(Some(false)),
        Some(_) => Err(error_response(
            StatusCode::BAD_REQUEST,
            format!("{param} must be true or false"),
            "invalid_request_error",
            Some(param),
        )),
    }
}
