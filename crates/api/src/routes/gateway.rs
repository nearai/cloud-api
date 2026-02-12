use crate::{middleware::auth::AuthenticatedApiKey, models::ErrorResponse};
use axum::{http::StatusCode, response::Json as ResponseJson, Extension};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Response from the check_api_key endpoint
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CheckApiKeyResponse {
    /// Whether the API key is valid and authorized
    pub valid: bool,
}

/// Check API key validity
///
/// Validates the provided API key (via Bearer token), checks rate limits,
/// and verifies the organization has sufficient credits.
///
/// This endpoint is designed for external model gateways to authenticate
/// user requests before forwarding to inference engines.
#[utoipa::path(
    post,
    path = "/v1/check_api_key",
    tag = "Gateway",
    responses(
        (status = 200, description = "API key is valid and has sufficient credits", body = CheckApiKeyResponse),
        (status = 401, description = "Invalid or missing API key", body = ErrorResponse),
        (status = 402, description = "Insufficient credits or spend limit exceeded", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("api_key" = [])
    )
)]
pub async fn check_api_key(
    Extension(_api_key): Extension<AuthenticatedApiKey>,
) -> Result<ResponseJson<CheckApiKeyResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    Ok(ResponseJson(CheckApiKeyResponse { valid: true }))
}
