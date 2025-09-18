use axum::http::StatusCode;
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
