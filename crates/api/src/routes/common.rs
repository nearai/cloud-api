use axum::http::StatusCode;
use domain::CompletionError;

/// Map domain errors to HTTP status codes
pub fn map_domain_error_to_status(error: &CompletionError) -> StatusCode {
    match error {
        CompletionError::InvalidModel(_) | CompletionError::InvalidParams(_) => StatusCode::BAD_REQUEST,
        CompletionError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        CompletionError::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
