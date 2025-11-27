use crate::models::ErrorResponse;
use axum::{http::StatusCode, response::Json as ResponseJson};
use services::completions::CompletionError;
use services::organization::OrganizationError;

/// Map domain errors to HTTP status codes
pub fn map_domain_error_to_status(error: &CompletionError) -> StatusCode {
    match error {
        CompletionError::InvalidModel(_) | CompletionError::InvalidParams(_) => {
            StatusCode::BAD_REQUEST
        }
        CompletionError::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
        CompletionError::ProviderError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CompletionError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Validate pagination parameters (limit/offset pattern)
///
/// Ensures:
/// - limit is positive (> 0)
/// - limit does not exceed 1000
/// - offset is non-negative (>= 0)
pub fn validate_limit_offset(
    limit: i64,
    offset: i64,
) -> Result<(), (StatusCode, ResponseJson<ErrorResponse>)> {
    if limit <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Limit must be positive".to_string(),
                "invalid_parameter".to_string(),
            )),
        ));
    }
    if limit > 1000 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Limit cannot exceed 1000".to_string(),
                "invalid_parameter".to_string(),
            )),
        ));
    }
    if offset < 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Offset must be non-negative".to_string(),
                "invalid_parameter".to_string(),
            )),
        ));
    }
    Ok(())
}

pub fn default_limit() -> i64 {
    100
}

/// Map OrganizationError to HTTP response
pub fn map_organization_error(
    error: OrganizationError,
) -> (StatusCode, ResponseJson<ErrorResponse>) {
    match error {
        OrganizationError::NotFound => (
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        ),
        OrganizationError::UserNotFound => (
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "User not found".to_string(),
                "not_found".to_string(),
            )),
        ),
        OrganizationError::Unauthorized(msg) => (
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(msg, "forbidden".to_string())),
        ),
        OrganizationError::InvalidParams(msg) => (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(msg, "bad_request".to_string())),
        ),
        OrganizationError::AlreadyExists => (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "Organization already exists".to_string(),
                "conflict".to_string(),
            )),
        ),
        OrganizationError::AlreadyMember => (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "User is already a member".to_string(),
                "conflict".to_string(),
            )),
        ),
        OrganizationError::InternalError(msg) => {
            tracing::error!("Organization internal error: {}", msg);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Internal server error".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        }
    }
}
