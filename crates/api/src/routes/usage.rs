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
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct OrganizationBalanceResponse {
    pub organization_id: String,
    pub total_spent_amount: i64,
    pub total_spent_scale: i32,
    pub total_spent_currency: String,
    pub total_spent_display: String, // Human readable, e.g., "12.50 USD"
    pub last_usage_at: Option<String>,
    pub total_requests: i64,
    pub total_tokens: i64,
    pub updated_at: String,
}

/// Usage history entry
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UsageHistoryEntryResponse {
    pub id: String,
    pub workspace_id: String,
    pub api_key_id: String,
    pub model_id: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub total_tokens: i32,
    pub total_cost_amount: i64,
    pub total_cost_scale: i32,
    pub total_cost_currency: String,
    pub total_cost_display: String, // Human readable
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
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    100
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
        ("bearer" = [])
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

    match balance {
        Some(balance) => Ok(ResponseJson(OrganizationBalanceResponse {
            organization_id: balance.organization_id.to_string(),
            total_spent_amount: balance.total_spent_amount,
            total_spent_scale: balance.total_spent_scale,
            total_spent_currency: balance.total_spent_currency.clone(),
            total_spent_display: format_amount(
                balance.total_spent_amount,
                balance.total_spent_scale,
                &balance.total_spent_currency,
            ),
            last_usage_at: balance.last_usage_at.map(|dt| dt.to_rfc3339()),
            total_requests: balance.total_requests,
            total_tokens: balance.total_tokens,
            updated_at: balance.updated_at.to_rfc3339(),
        })),
        None => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "No usage data found for organization".to_string(),
                "not_found".to_string(),
            )),
        )),
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
        ("bearer" = [])
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

    let history = app_state
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

    let total = history.len();
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
            total_cost_amount: entry.total_cost_amount,
            total_cost_scale: entry.total_cost_scale,
            total_cost_currency: entry.total_cost_currency.clone(),
            total_cost_display: format_amount(
                entry.total_cost_amount,
                entry.total_cost_scale,
                &entry.total_cost_currency,
            ),
            request_type: entry.request_type,
            created_at: entry.created_at.to_rfc3339(),
        })
        .collect();

    Ok(ResponseJson(UsageHistoryResponse {
        data,
        total,
        limit: query.limit,
        offset: query.offset,
    }))
}

/// Helper function to format amount with currency
fn format_amount(amount: i64, scale: i32, currency: &str) -> String {
    let divisor = 10_i64.pow(scale as u32);
    let whole = amount / divisor;
    let fraction = amount % divisor;

    if fraction == 0 {
        format!("{} {}", whole, currency)
    } else {
        // Remove trailing zeros from fraction
        let fraction_str = format!("{:0width$}", fraction, width = scale as usize);
        let trimmed = fraction_str.trim_end_matches('0');
        format!("{}.{} {}", whole, trimmed, currency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_amount() {
        assert_eq!(format_amount(1000000, 6, "USD"), "1 USD");
        assert_eq!(format_amount(1500000, 6, "USD"), "1.5 USD");
        assert_eq!(format_amount(1230000, 6, "USD"), "1.23 USD");
        assert_eq!(format_amount(100, 6, "USD"), "0.0001 USD");
        assert_eq!(format_amount(1, 6, "USD"), "0.000001 USD");
    }
}
