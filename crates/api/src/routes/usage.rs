use crate::{middleware::AuthenticatedUser, models::ErrorResponse, routes::api::AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Get organization balance response
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrganizationBalanceResponse {
    pub organization_id: String,
    pub total_spent: i64,            // In nano-dollars (scale 9)
    pub total_spent_display: String, // Human readable, e.g., "$12.50"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit: Option<i64>, // In nano-dollars (scale 9), None if not set
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_limit_display: Option<String>, // Human readable, e.g., "$100.00"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining: Option<i64>, // Remaining credits in nano-dollars
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_display: Option<String>, // Human readable remaining
    pub last_usage_at: Option<String>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: String,
}

/// Usage history entry
/// All costs use fixed scale of 9 (nano-dollars) and USD currency
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryEntryResponse {
    pub id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub total_cost: i64,            // In nano-dollars (scale 9)
    pub total_cost_display: String, // Human readable, e.g., "$0.00123"
    pub request_type: String,
    pub created_at: String,
}

/// Usage history response
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryResponse {
    pub data: Vec<UsageHistoryEntryResponse>,
    pub total: usize,
    pub limit: i64,
    pub offset: i64,
}

/// Query parameters for usage history
#[derive(Debug, Deserialize)]
pub struct UsageHistoryQuery {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

/// Get organization balance
///
/// Returns the current spending balance for an organization
#[utoipa::path(
    get,
    path = "/organizations/{org_id}/usage/balance",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization balance", body = OrganizationBalanceResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_balance(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> Result<ResponseJson<OrganizationBalanceResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get balance request for org {} by user {}",
        org_id,
        user.0.id
    );

    let organization_id = Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // TODO: Check if user has access to this organization
    // For now, we assume the user is authenticated and can access their own orgs

    let balance = app_state
        .usage_service
        .get_balance(organization_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get balance: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve balance".to_string(),
                    "internal_error".to_string(),
                )),
            )
        })?;

    // Get spending limit
    let limit = app_state
        .usage_service
        .get_limit(organization_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get limit: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve limit".to_string(),
                    "internal_error".to_string(),
                )),
            )
        })?;

    match balance {
        Some(balance) => {
            let (spend_limit, spend_limit_display, remaining, remaining_display) =
                if let Some(limit_info) = limit {
                    let remaining_amount = limit_info.spend_limit - balance.total_spent;
                    (
                        Some(limit_info.spend_limit),
                        Some(format_amount(limit_info.spend_limit)),
                        Some(remaining_amount),
                        Some(format_amount(remaining_amount)),
                    )
                } else {
                    (None, None, None, None)
                };

            Ok(ResponseJson(OrganizationBalanceResponse {
                organization_id: balance.organization_id.to_string(),
                total_spent: balance.total_spent,
                total_spent_display: format_amount(balance.total_spent),
                spend_limit,
                spend_limit_display,
                remaining,
                remaining_display,
                last_usage_at: balance.last_usage_at.map(|dt| dt.to_rfc3339()),
                total_requests: balance.total_requests,
                total_tokens: balance.total_tokens,
                updated_at: balance.updated_at.to_rfc3339(),
            }))
        }
        None => {
            // No balance yet, but may have a limit
            if let Some(limit_info) = limit {
                // Organization has a limit but no usage yet - return with zero balance
                Ok(ResponseJson(OrganizationBalanceResponse {
                    organization_id: org_id,
                    total_spent: 0,
                    total_spent_display: format_amount(0),
                    spend_limit: Some(limit_info.spend_limit),
                    spend_limit_display: Some(format_amount(limit_info.spend_limit)),
                    remaining: Some(limit_info.spend_limit),
                    remaining_display: Some(format_amount(limit_info.spend_limit)),
                    last_usage_at: None,
                    total_requests: 0,
                    total_tokens: 0,
                    updated_at: chrono::Utc::now().to_rfc3339(),
                }))
            } else {
                Err((
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "No usage data or limit found for organization".to_string(),
                        "not_found".to_string(),
                    )),
                ))
            }
        }
    }
}

/// Get organization usage history
///
/// Returns paginated usage history for an organization
#[utoipa::path(
    get,
    path = "/organizations/{org_id}/usage/history",
    tag = "Usage",
    params(
        ("org_id" = String, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)")
    ),
    responses(
        (status = 200, description = "Usage history", body = UsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    Query(query): Query<UsageHistoryQuery>,
) -> Result<ResponseJson<UsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get usage history for org {} by user {}, limit: {}, offset: {}",
        org_id,
        user.0.id,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;

    let organization_id = Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // TODO: Check if user has access to this organization

    let (history, total) = app_state
        .usage_service
        .get_usage_history(organization_id, Some(query.limit), Some(query.offset))
        .await
        .map_err(|e| {
            tracing::error!("Failed to get usage history: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to retrieve usage history".to_string(),
                    "internal_error".to_string(),
                )),
            )
        })?;

    let data = history
        .into_iter()
        .map(|entry| UsageHistoryEntryResponse {
            id: entry.id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            model_id: entry.model_id,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            total_tokens: entry.total_tokens,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            request_type: entry.request_type,
            created_at: entry.created_at.to_rfc3339(),
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

/// Get API key usage history
///
/// Returns paginated usage history for a specific API key
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}/api-keys/{api_key_id}/usage/history",
    tag = "Usage",
    params(
        ("workspace_id" = String, Path, description = "Workspace ID"),
        ("api_key_id" = String, Path, description = "API Key ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)")
    ),
    responses(
        (status = 200, description = "Usage history", body = UsageHistoryResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_api_key_usage_history(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(String, String)>,
    Query(query): Query<UsageHistoryQuery>,
) -> Result<ResponseJson<UsageHistoryResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    tracing::debug!(
        "Get usage history for API key {} in workspace {} by user {}, limit: {}, offset: {}",
        api_key_id,
        workspace_id,
        user.0.id,
        query.limit,
        query.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(query.limit, query.offset)?;

    let workspace_uuid = Uuid::parse_str(&workspace_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid workspace ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let api_key_uuid = Uuid::parse_str(&api_key_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid API key ID".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    // Get usage history with permission checking handled by the service
    let (history, total) = app_state
        .usage_service
        .get_api_key_usage_history_with_permissions(
            workspace_uuid,
            api_key_uuid,
            user.0.id,
            Some(query.limit),
            Some(query.offset),
        )
        .await
        .map_err(|e| {
            tracing::error!("Failed to get usage history: {}", e);
            match e {
                services::usage::UsageError::Unauthorized(_) => (
                    StatusCode::FORBIDDEN,
                    ResponseJson(ErrorResponse::new(
                        "Access denied to this workspace".to_string(),
                        "forbidden".to_string(),
                    )),
                ),
                services::usage::UsageError::NotFound(_) => (
                    StatusCode::NOT_FOUND,
                    ResponseJson(ErrorResponse::new(
                        "API key not found in this workspace".to_string(),
                        "not_found".to_string(),
                    )),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ResponseJson(ErrorResponse::new(
                        "Failed to retrieve usage history".to_string(),
                        "internal_error".to_string(),
                    )),
                ),
            }
        })?;

    let data = history
        .into_iter()
        .map(|entry| UsageHistoryEntryResponse {
            id: entry.id.to_string(),
            workspace_id: entry.workspace_id.to_string(),
            api_key_id: entry.api_key_id.to_string(),
            model_id: entry.model_id,
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
            total_tokens: entry.total_tokens,
            total_cost: entry.total_cost,
            total_cost_display: format_amount(entry.total_cost),
            request_type: entry.request_type,
            created_at: entry.created_at.to_rfc3339(),
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total: total as usize,
        limit: query.limit,
        offset: query.offset,
    }))
}

/// Helper function to format amount (fixed scale 9 = nano-dollars, USD)
fn format_amount(amount: i64) -> String {
    const SCALE: i64 = 9;
    let divisor = 10_i64.pow(SCALE as u32);
    let whole = amount / divisor;
    let fraction = amount % divisor;

    if fraction == 0 {
        format!("${}.00", whole)
    } else {
        // Remove trailing zeros from fraction
        let fraction_str = format!("{:09}", fraction);
        let trimmed = fraction_str.trim_end_matches('0');
        format!("${}.{}", whole, trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_amount() {
        // Test with scale 9 (nano-dollars, USD)
        assert_eq!(format_amount(1000000000), "$1.00");
        assert_eq!(format_amount(1500000000), "$1.5");
        assert_eq!(format_amount(1230000000), "$1.23");
        assert_eq!(format_amount(100000), "$0.0001");
        assert_eq!(format_amount(1), "$0.000000001");
        assert_eq!(format_amount(0), "$0.00");
    }
}
