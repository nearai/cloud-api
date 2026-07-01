use crate::models::ErrorResponse;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Json as ResponseJson, Response},
};
use services::attestation::AttestationError;

pub(super) fn signature_error_response(
    error: AttestationError,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
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
        | AttestationError::InternalError(_)
        | AttestationError::ItaUnavailable { .. }
        | AttestationError::ItaRateLimited { .. }
        | AttestationError::ItaTimeout
        | AttestationError::ItaBadUpstream { .. }
        | AttestationError::ItaInvalidEvidence { .. } => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            message,
            "internal_server_error",
            None,
        ),
    }
}

pub(super) fn attestation_report_error_response(
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
        AttestationError::RepositoryError(_)
        | AttestationError::InternalError(_)
        | AttestationError::ItaUnavailable { .. }
        | AttestationError::ItaRateLimited { .. }
        | AttestationError::ItaTimeout
        | AttestationError::ItaBadUpstream { .. }
        | AttestationError::ItaInvalidEvidence { .. } => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            message,
            "internal_server_error",
            None,
        ),
    }
}

pub(super) fn ita_token_error_response(error: AttestationError) -> Response {
    let message = error.to_string();
    match &error {
        AttestationError::InvalidParameter(detail) => error_tuple_into_response(error_response(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            invalid_parameter_name(detail),
        )),
        AttestationError::ClientError(_) => error_tuple_into_response(error_response(
            StatusCode::BAD_REQUEST,
            message,
            "invalid_request_error",
            None,
        )),
        AttestationError::ProviderError(_) | AttestationError::ItaInvalidEvidence { .. } => {
            error_tuple_into_response(error_response(
                StatusCode::BAD_REQUEST,
                message,
                "invalid_request_error",
                None,
            ))
        }
        AttestationError::ItaRateLimited { retry_after } => {
            let mut response = error_tuple_into_response(error_response(
                StatusCode::TOO_MANY_REQUESTS,
                message,
                "rate_limit_error",
                None,
            ));
            if let Some(retry_after) = retry_after {
                if let Ok(value) = axum::http::HeaderValue::from_str(retry_after) {
                    response
                        .headers_mut()
                        .insert(axum::http::header::RETRY_AFTER, value);
                }
            }
            response
        }
        AttestationError::ItaBadUpstream { .. } => error_tuple_into_response(error_response(
            StatusCode::BAD_GATEWAY,
            message,
            "bad_gateway",
            None,
        )),
        AttestationError::ItaUnavailable { .. } => error_tuple_into_response(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            message,
            "service_unavailable",
            None,
        )),
        AttestationError::ItaTimeout => error_tuple_into_response(error_response(
            StatusCode::GATEWAY_TIMEOUT,
            message,
            "timeout_error",
            None,
        )),
        AttestationError::SignatureNotFound(_) => error_tuple_into_response(error_response(
            StatusCode::NOT_FOUND,
            message,
            "not_found_error",
            None,
        )),
        AttestationError::RepositoryError(_) | AttestationError::InternalError(_) => {
            error_tuple_into_response(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                message,
                "internal_server_error",
                None,
            ))
        }
    }
}

pub(super) fn error_tuple_into_response(
    error: (StatusCode, ResponseJson<ErrorResponse>),
) -> Response {
    error.into_response()
}

pub(super) fn internal_error_response(
    message: String,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        message,
        "internal_server_error",
        None,
    )
}

pub(super) fn error_response(
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
    } else if message.contains("policy_ids") || message.contains("policy ids") {
        Some("policy_ids")
    } else if message.contains("policy_must_match") {
        Some("policy_must_match")
    } else if message.contains("token_signing_alg") || message.contains("ita_token_signing_alg") {
        Some("token_signing_alg")
    } else if message.contains("model") {
        Some("model")
    } else {
        None
    }
}
