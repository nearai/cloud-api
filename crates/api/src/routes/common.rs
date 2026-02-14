use crate::models::ErrorResponse;
use axum::{http::HeaderMap, http::StatusCode, response::Json as ResponseJson};
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
        CompletionError::ServiceOverloaded(_) => StatusCode::SERVICE_UNAVAILABLE,
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

/// Basic non-empty string validation helper
pub fn validate_non_empty_field(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{field} cannot be empty"));
    }
    Ok(())
}

/// Basic max-length string validation helper
pub fn validate_max_length(value: &str, field: &str, max: usize) -> Result<(), String> {
    if value.len() > max {
        return Err(format!("{field} is too long (max {max} bytes)"));
    }
    Ok(())
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

/// Helper function to format amount (fixed scale 9 = nano-dollars, USD)
pub fn format_amount(amount: i64) -> String {
    const SCALE: i64 = 9;
    let divisor = 10_i64.pow(SCALE as u32);
    let whole = amount / divisor;
    let frac = amount % divisor;

    if frac == 0 {
        format!("${whole}.00")
    } else {
        // Remove trailing zeros from fractional part
        let frac_str = format!("{:0>9}", frac.abs());
        let trimmed = frac_str.trim_end_matches('0');
        format!("${whole}.{trimmed}")
    }
}

/// Validated encryption headers extracted from HTTP request
#[derive(Debug, Clone)]
pub struct EncryptionHeaders {
    pub signing_algo: Option<String>,
    pub client_pub_key: Option<String>,
    pub model_pub_key: Option<String>,
}

/// Validate and extract encryption headers from HTTP request
///
/// Validates:
/// - `x-signing-algo`: Must be "ecdsa" or "ed25519" (case-insensitive)
/// - `x-client-pub-key`: Must be a valid hex string with correct length based on algorithm
///   - Ed25519: 64 hex characters (32 bytes)
///   - ECDSA: 128 hex characters (64 bytes) or 130 hex characters (65 bytes with 0x04 prefix)
/// - `x-model-pub-key`: Must be a valid hex string (reasonable length: 64-130 hex characters)
///
/// Returns:
/// - `Ok(EncryptionHeaders)` if all provided headers are valid
/// - `Err((StatusCode, ResponseJson<ErrorResponse>))` if validation fails
pub fn validate_encryption_headers(
    headers: &HeaderMap,
) -> Result<EncryptionHeaders, (StatusCode, ResponseJson<ErrorResponse>)> {
    // Extract headers (case-insensitive)
    let signing_algo = headers
        .get("x-signing-algo")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let client_pub_key = headers
        .get("x-client-pub-key")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());
    let model_pub_key = headers
        .get("x-model-pub-key")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    // Validate signing algorithm if provided
    if let Some(ref algo) = signing_algo {
        let algo_lower = algo.to_lowercase();
        if algo_lower != "ecdsa" && algo_lower != "ed25519" {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!(
                        "Invalid X-Signing-Algo: '{}'. Must be 'ecdsa' or 'ed25519'",
                        algo
                    ),
                    "invalid_parameter".to_string(),
                )),
            ));
        }
    }

    // Validate client public key if provided
    if let Some(ref pub_key) = client_pub_key {
        // Check if it's a valid hex string
        let pub_key_bytes = match hex::decode(pub_key) {
            Ok(bytes) => bytes,
            Err(_) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "X-Client-Pub-Key must be a valid hex string".to_string(),
                        "invalid_parameter".to_string(),
                    )),
                ));
            }
        };

        // If signing_algo is provided, validate length based on algorithm
        if let Some(ref algo) = signing_algo {
            let algo_lower = algo.to_lowercase();
            if algo_lower == "ed25519" {
                // Ed25519: 32 bytes = 64 hex characters
                if pub_key_bytes.len() != 32 {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!(
                                "Ed25519 public key must be 64 hex characters (32 bytes), got {} characters",
                                pub_key.len()
                            ),
                            "invalid_parameter".to_string(),
                        )),
                    ));
                }
            } else if algo_lower == "ecdsa" {
                // ECDSA: 64 bytes (128 hex chars) or 65 bytes with 0x04 prefix (130 hex chars)
                if pub_key_bytes.len() == 65 && pub_key_bytes[0] == 0x04 {
                    // Uncompressed format with 0x04 prefix - valid
                } else if pub_key_bytes.len() == 64 {
                    // Uncompressed format without 0x04 prefix - valid
                } else {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!(
                                "ECDSA public key must be 128 hex characters (64 bytes) or 130 hex characters (65 bytes with 0x04 prefix), got {} characters",
                                pub_key.len()
                            ),
                            "invalid_parameter".to_string(),
                        )),
                    ));
                }
            }
        } else {
            // If no signing_algo provided, just check it's a reasonable length
            // (between 64 and 130 hex characters, which covers both Ed25519 and ECDSA)
            if pub_key.len() < 64 || pub_key.len() > 130 {
                return Err((
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        format!(
                            "X-Client-Pub-Key must be between 64 and 130 hex characters, got {} characters",
                            pub_key.len()
                        ),
                        "invalid_parameter".to_string(),
                    )),
                ));
            }
        }
    }

    // Validate model public key if provided
    if let Some(ref pub_key) = model_pub_key {
        // Check if it's a valid hex string
        if hex::decode(pub_key).is_err() {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    "X-Model-Pub-Key must be a valid hex string".to_string(),
                    "invalid_parameter".to_string(),
                )),
            ));
        }

        // Check reasonable length (64-130 hex characters, which covers both Ed25519 and ECDSA)
        if pub_key.len() < 64 || pub_key.len() > 130 {
            return Err((
                StatusCode::BAD_REQUEST,
                ResponseJson(ErrorResponse::new(
                    format!(
                        "X-Model-Pub-Key must be between 64 and 130 hex characters, got {} characters",
                        pub_key.len()
                    ),
                    "invalid_parameter".to_string(),
                )),
            ));
        }
    }

    Ok(EncryptionHeaders {
        signing_algo,
        client_pub_key,
        model_pub_key,
    })
}

/// Redact sensitive fields (e.g. `api_key`) from a `provider_config` JSON value.
///
/// Replaces `api_key` with `"***"` so callers can tell whether one is configured
/// without exposing the actual value.
pub fn redact_provider_config(config: Option<serde_json::Value>) -> Option<serde_json::Value> {
    config.map(|mut v| {
        if let Some(obj) = v.as_object_mut() {
            if obj.contains_key("api_key") {
                obj.insert(
                    "api_key".to_string(),
                    serde_json::Value::String("***".to_string()),
                );
            }
        }
        v
    })
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

    #[test]
    fn test_format_amount() {
        // Test with scale 9 (nano-dollars, USD)
        assert_eq!(format_amount(1000000000), "$1.00");
        assert_eq!(format_amount(1500000000), "$1.5");
        assert_eq!(format_amount(1230000000), "$1.23");
        assert_eq!(format_amount(100000), "$0.0001");
        assert_eq!(format_amount(100), "$0.0000001");
        assert_eq!(format_amount(1), "$0.000000001");
        assert_eq!(format_amount(0), "$0.00");
    }
}
