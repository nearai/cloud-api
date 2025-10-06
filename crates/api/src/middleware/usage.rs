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
                "Organization {} exceeded credit limit. Spent: {}, Limit: {}",
                organization_id,
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
    }
}

/// Helper function to format amount (fixed scale 9 = nano-dollars, USD)
fn format_amount(amount: i64) -> String {
    const SCALE: i32 = 9;
    let divisor = 10_i64.pow(SCALE as u32);
    let whole = amount / divisor;
    let fraction = amount % divisor;

    if fraction == 0 {
        format!("${}.00", whole)
    } else {
        // Format with leading zeros, then trim trailing zeros
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
        // Test with scale 9 (nano-dollars)
        assert_eq!(format_amount(1000000000), "$1.00");
        assert_eq!(format_amount(1500000000), "$1.5");
        assert_eq!(format_amount(100), "$0.0000001");
        assert_eq!(format_amount(1), "$0.000000001");
        assert_eq!(format_amount(0), "$0.00");
    }
}
