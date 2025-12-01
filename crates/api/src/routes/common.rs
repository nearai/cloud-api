use crate::models::ErrorResponse;
use axum::{http::StatusCode, response::Json as ResponseJson};
use services::completions::CompletionError;
use services::organization::OrganizationError;
use uuid::Uuid;

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

/// Validates and parses a file ID reference from legacy text format.
///
/// Legacy format: "[File: file-{uuid}]" or "[File: {uuid}]"
///
/// Returns:
/// - Ok(Some(file_id)) if valid file reference with proper UUID format
/// - Ok(None) if text doesn't match the file reference pattern
/// - Err(msg) if pattern matches but UUID validation fails
///
/// This allows the caller to distinguish between:
/// - Regular text that should be treated as-is (Ok(None))
/// - Malformed file references that might indicate data issues (Err)
pub fn parse_legacy_file_reference(text: &str) -> Result<Option<String>, String> {
    // Check if text matches the legacy file reference pattern
    let file_id = match text
        .strip_prefix("[File: ")
        .and_then(|s| s.strip_suffix("]"))
    {
        Some(id) => id,
        None => return Ok(None),
    };

    // Extract UUID string (with or without "file-" prefix for backward compatibility)
    let uuid_str = file_id.strip_prefix("file-").unwrap_or(file_id);

    // Validate UUID format - if invalid, return error
    Uuid::parse_str(uuid_str).map_err(|_| format!("Invalid UUID in file reference: {uuid_str}"))?;

    // Return the full file ID (preserving original format with or without prefix)
    Ok(Some(file_id.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_legacy_file_reference_valid_with_prefix() {
        let result =
            parse_legacy_file_reference("[File: file-32af7670-f5b9-47a0-a952-20d5d3831e67]");
        assert_eq!(
            result.unwrap(),
            Some("file-32af7670-f5b9-47a0-a952-20d5d3831e67".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_file_reference_valid_no_prefix() {
        // Backward compatibility: accept UUID without "file-" prefix
        let result = parse_legacy_file_reference("[File: 32af7670-f5b9-47a0-a952-20d5d3831e67]");
        assert_eq!(
            result.unwrap(),
            Some("32af7670-f5b9-47a0-a952-20d5d3831e67".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_file_reference_invalid_uuid() {
        // Invalid UUID should return Err
        let result = parse_legacy_file_reference("[File: file-invalid]");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid UUID"));
    }

    #[test]
    fn test_parse_legacy_file_reference_not_file() {
        let result = parse_legacy_file_reference("Just some text");
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_parse_legacy_file_reference_looks_like_file() {
        // Doesn't match exact pattern (missing space or brackets)
        let result = parse_legacy_file_reference(
            "Discussion about [file-32af7670f5b947a0a95220d5d3831e67] in text",
        );
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_parse_legacy_file_reference_missing_brackets() {
        let result = parse_legacy_file_reference("file-32af7670-f5b9-47a0-a952-20d5d3831e67");
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_parse_legacy_file_reference_malformed_uuid() {
        // UUID without hyphens is actually accepted by Uuid::parse_str (simple format)
        let result = parse_legacy_file_reference("[File: 32af7670f5b947a0a95220d5d3831e67]");
        assert_eq!(
            result.unwrap(),
            Some("32af7670f5b947a0a95220d5d3831e67".to_string())
        );
    }

    #[test]
    fn test_parse_legacy_file_reference_truly_invalid_uuid() {
        // Truly invalid UUID should fail
        let result = parse_legacy_file_reference("[File: not-a-uuid-at-all]");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid UUID"));
    }

    #[test]
    fn test_validate_limit_offset() {
        // Valid cases
        assert!(validate_limit_offset(10, 0).is_ok());
        assert!(validate_limit_offset(1000, 100).is_ok());

        // Invalid limit <= 0
        let err = validate_limit_offset(0, 0).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1 .0.error.message, "Limit must be positive");
        assert_eq!(err.1 .0.error.r#type, "invalid_parameter");

        let err = validate_limit_offset(-1, 0).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1 .0.error.message, "Limit must be positive");

        // Invalid limit > 1000
        let err = validate_limit_offset(1001, 0).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1 .0.error.message, "Limit cannot exceed 1000");
        assert_eq!(err.1 .0.error.r#type, "invalid_parameter");

        // Invalid offset < 0
        let err = validate_limit_offset(10, -1).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(err.1 .0.error.message, "Offset must be non-negative");
        assert_eq!(err.1 .0.error.r#type, "invalid_parameter");
    }
}
