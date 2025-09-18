use thiserror::Error;

#[derive(Debug, Error)]
pub enum CompletionError {
    #[error("Invalid model: {0}")]
    InvalidModel(String),
    #[error("Invalid parameters: {0}")]
    InvalidParams(String),
    #[error("Rate limited")]
    RateLimited,
    #[error("Internal error: {0}")]
    InternalError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_messages() {
        // Test all error variants have proper messages
        let invalid_model = CompletionError::InvalidModel("gpt-invalid".to_string());
        assert_eq!(invalid_model.to_string(), "Invalid model: gpt-invalid");

        let invalid_params = CompletionError::InvalidParams("missing required field".to_string());
        assert_eq!(invalid_params.to_string(), "Invalid parameters: missing required field");

        let rate_limited = CompletionError::RateLimited;
        assert_eq!(rate_limited.to_string(), "Rate limited");

        let internal_error = CompletionError::InternalError("database connection failed".to_string());
        assert_eq!(internal_error.to_string(), "Internal error: database connection failed");
    }
}
