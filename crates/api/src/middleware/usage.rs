use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use services::usage::{UsageCheckResult, UsageServiceTrait};
use std::sync::Arc;
use tracing::{debug, warn};

use super::auth::AuthenticatedApiKey;
use crate::models::ErrorResponse;
use crate::routes::common::format_amount;

/// State for usage middleware
#[derive(Clone)]
pub struct UsageState {
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub usage_repository: Arc<database::repositories::OrganizationUsageRepository>,
    pub api_key_repository: Arc<database::repositories::ApiKeyRepository>,
}

/// Middleware to check if organization has sufficient credits before processing request
pub async fn usage_check_middleware(
    State(state): State<UsageState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<ErrorResponse>)> {
    // Extract organization from authenticated API key
    let api_key = request
        .extensions()
        .get::<AuthenticatedApiKey>()
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new(
                    "API key authentication required".to_string(),
                    "unauthorized".to_string(),
                )),
            )
        })?;

    let organization_id = api_key.organization.id.0;
    let api_key_id = api_key.api_key.id.clone();

    debug!(
        "Checking usage limits for organization: {} and API key: {}",
        organization_id, api_key_id.0
    );

    // First, check API key spend limit if one is set
    if let Some(api_key_limit) = api_key.api_key.spend_limit {
        let api_key_uuid = uuid::Uuid::parse_str(&api_key_id.0).map_err(|_| {
            tracing::error!("Failed to parse API key ID");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Internal error".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

        let api_key_spend = state
            .usage_repository
            .get_api_key_spend(api_key_uuid)
            .await
            .map_err(|_| {
                tracing::error!("Failed to get API key spend");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(ErrorResponse::new(
                        "Failed to check API key spend".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            })?;

        if api_key_spend >= api_key_limit {
            warn!(
                "API key exceeded spend limit. Spent: {}, Limit: {}",
                format_amount(api_key_spend),
                format_amount(api_key_limit)
            );
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    format!(
                        "API key spend limit exceeded. Spent: {}, Limit: {}",
                        format_amount(api_key_spend),
                        format_amount(api_key_limit)
                    ),
                    "api_key_limit_exceeded".to_string(),
                )),
            ));
        }

        debug!(
            "API key {} within spend limit. Spent: {}, Limit: {}, Remaining: {}",
            api_key_id.0,
            format_amount(api_key_spend),
            format_amount(api_key_limit),
            format_amount(api_key_limit - api_key_spend)
        );
    }

    // Check if organization can make request
    let check_result = state
        .usage_service
        .check_can_use(organization_id)
        .await
        .map_err(|_| {
            tracing::error!("Failed to check usage limits");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Failed to check usage limits".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    match check_result {
        UsageCheckResult::Allowed { remaining } => {
            debug!(
                "Organization {} has sufficient credits. Remaining: {}",
                organization_id,
                format_amount(remaining)
            );
            Ok(next.run(request).await)
        }
        UsageCheckResult::LimitExceeded { spent, limit } => {
            warn!(
                "Organization exceeded credit limit. Spent: {}, Limit: {}",
                format_amount(spent),
                format_amount(limit)
            );
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    format!(
                        "Credit limit exceeded. Spent: {}, Limit: {}. Please purchase more credits.",
                        format_amount(spent),
                        format_amount(limit)
                    ),
                    "insufficient_credits".to_string(),
                )),
            ))
        }
        UsageCheckResult::NoCredits => {
            warn!("Organization has no credits - denying request");
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    "No credits available. Please purchase credits to use the API.".to_string(),
                    "no_credits".to_string(),
                )),
            ))
        }
        UsageCheckResult::NoLimitSet => {
            warn!("Organization has no spending limit configured - denying request");
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    "No spending limit configured. Please contact support to set up credits."
                        .to_string(),
                    "no_limit_configured".to_string(),
                )),
            ))
        }
    }
}
