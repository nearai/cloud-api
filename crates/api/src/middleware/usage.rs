use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use services::usage::{UsageCheckResult, UsageService};
use std::sync::Arc;
use tracing::{debug, warn};

use super::auth::AuthenticatedApiKey;
use crate::models::ErrorResponse;

/// State for usage middleware
#[derive(Clone)]
pub struct UsageState {
    pub usage_service: Arc<dyn UsageService + Send + Sync>,
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
    debug!(
        "Checking usage limits for organization: {}",
        organization_id
    );

    // Check if organization can make request
    let check_result = state
        .usage_service
        .check_can_use(organization_id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check usage limits: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Failed to check usage limits".to_string(),
                    "internal_error".to_string(),
                )),
            )
        })?;

    match check_result {
        UsageCheckResult::Allowed {
            remaining_amount,
            remaining_scale,
            remaining_currency,
        } => {
            debug!(
                "Organization {} has sufficient credits. Remaining: {} (scale: {}, currency: {})",
                organization_id, remaining_amount, remaining_scale, remaining_currency
            );
            Ok(next.run(request).await)
        }
        UsageCheckResult::LimitExceeded {
            spent_amount,
            spent_scale,
            spent_currency,
            limit_amount,
            limit_scale,
            limit_currency,
        } => {
            warn!(
                "Organization {} exceeded credit limit. Spent: {} {}, Limit: {} {}",
                organization_id,
                format_amount(spent_amount, spent_scale),
                spent_currency,
                format_amount(limit_amount, limit_scale),
                limit_currency
            );
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    format!(
                        "Credit limit exceeded. Spent: {} {}, Limit: {} {}. Please purchase more credits.",
                        format_amount(spent_amount, spent_scale),
                        spent_currency,
                        format_amount(limit_amount, limit_scale),
                        limit_currency
                    ),
                    "insufficient_credits".to_string(),
                )),
            ))
        }
        UsageCheckResult::NoCredits => {
            warn!(
                "Organization {} has no credits - denying request",
                organization_id
            );
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    "No credits available. Please purchase credits to use the API.".to_string(),
                    "no_credits".to_string(),
                )),
            ))
        }
        UsageCheckResult::NoLimitSet => {
            warn!(
                "Organization {} has no spending limit configured - denying request",
                organization_id
            );
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    "No spending limit configured. Please contact support to set up credits."
                        .to_string(),
                    "no_limit_configured".to_string(),
                )),
            ))
        }
        UsageCheckResult::CurrencyMismatch {
            spent_currency,
            limit_currency,
        } => {
            tracing::error!(
                "Currency mismatch for organization {}: spent in {}, limit in {}",
                organization_id,
                spent_currency,
                limit_currency
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    format!(
                        "Currency mismatch: spending tracked in {}, limit set in {}. Please contact support.",
                        spent_currency, limit_currency
                    ),
                    "currency_mismatch".to_string(),
                )),
            ))
        }
    }
}

/// Helper function to format amount with scale for display
fn format_amount(amount: i64, scale: i32) -> String {
    let divisor = 10_i64.pow(scale as u32);
    let whole = amount / divisor;
    let fraction = amount % divisor;

    if fraction == 0 {
        format!("{}", whole)
    } else {
        format!("{}.{:0width$}", whole, fraction, width = scale as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_amount() {
        // Test with scale 6 (micro-dollars)
        assert_eq!(format_amount(1000000, 6), "1");
        assert_eq!(format_amount(1500000, 6), "1.500000");
        assert_eq!(format_amount(100, 6), "0.000100");

        // Test with scale 2 (cents)
        assert_eq!(format_amount(100, 2), "1");
        assert_eq!(format_amount(150, 2), "1.50");
        assert_eq!(format_amount(1, 2), "0.01");

        // Test with scale 0
        assert_eq!(format_amount(100, 0), "100");
    }
}
