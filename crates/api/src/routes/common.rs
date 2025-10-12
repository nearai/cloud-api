use crate::models::ErrorResponse;
use axum::{http::StatusCode, response::Json as ResponseJson};
use services::CompletionError;

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
