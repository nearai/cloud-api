use crate::{middleware::auth::AuthenticatedApiKey, models::ErrorResponse};
use axum::{http::StatusCode, response::Json as ResponseJson, Extension};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Response from the check_api_key endpoint.
///
/// `org_id` and `workspace_id` are returned so downstream gateways
/// (e.g. inference-proxy) have a server-side authoritative tenant
/// identifier and don't have to trust caller-supplied `X-Org-Id`
/// headers when populating logs / usage records.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CheckApiKeyResponse {
    /// Whether the API key is valid and authorized
    pub valid: bool,
    /// Organization the API key belongs to
    pub org_id: String,
    /// Workspace the API key belongs to
    pub workspace_id: String,
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
    Extension(api_key): Extension<AuthenticatedApiKey>,
) -> Result<ResponseJson<CheckApiKeyResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    Ok(ResponseJson(CheckApiKeyResponse {
        valid: true,
        org_id: api_key.organization.id.to_string(),
        workspace_id: api_key.workspace.id.to_string(),
    }))
}
