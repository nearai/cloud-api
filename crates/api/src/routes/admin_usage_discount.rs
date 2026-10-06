//! Administrator-managed organization inference discounts.
use crate::{middleware::AdminUser, models::ErrorResponse};
use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use database::repositories::organization_usage_discount::{
    DiscountError, OrganizationUsageDiscount, OrganizationUsageDiscountRepository,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UsageDiscountRequest {
    /// Whole basis points (1 through 10000).
    pub discount_basis_points: i32,
    /// Inclusive historical cutoff. Omit to discount only new usage.
    pub apply_since: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct UsageDiscountResponse {
    pub discount_basis_points: i32,
    pub apply_since: Option<DateTime<Utc>>,
    pub saved_at: DateTime<Utc>,
    /// `applying` until historical correction finishes; otherwise `active`.
    pub status: String,
    pub processed_count: i64,
    /// Safe diagnostic while automatic retries are delayed.
    pub last_error: Option<String>,
    pub next_retry_at: Option<DateTime<Utc>>,
}
impl From<OrganizationUsageDiscount> for UsageDiscountResponse {
    fn from(rule: OrganizationUsageDiscount) -> Self {
        Self {
            discount_basis_points: rule.discount_basis_points,
            apply_since: rule.apply_since,
            saved_at: rule.saved_at,
            status: rule.status,
            processed_count: rule.processed_count,
            last_error: rule.last_error,
            next_retry_at: rule.next_retry_at,
        }
    }
}
type ApiError = (StatusCode, Json<ErrorResponse>);
fn api_error(error: anyhow::Error) -> ApiError {
    let (status, code, message) = match error.downcast_ref::<DiscountError>() {
        Some(DiscountError::Invalid) => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        ),
        Some(DiscountError::NotFound) => (StatusCode::NOT_FOUND, "not_found", error.to_string()),
        Some(DiscountError::Conflict | DiscountError::UnsupportedHistory) => {
            (StatusCode::CONFLICT, "conflict", error.to_string())
        }
        None => {
            tracing::error!(error = %error, "Organization usage discount operation failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_server_error",
                "Failed to save or read usage discount".into(),
            )
        }
    };
    (status, Json(ErrorResponse::new(message, code.into())))
}

#[utoipa::path(get, path="/v1/admin/organizations/{org_id}/usage-discount", tag="Admin",
    params(("org_id" = Uuid, Path, description="Organization ID")),
    responses((status=200, description="Discount or null when unset", body=Option<UsageDiscountResponse>),
        (status=401, description="Unauthorized", body=ErrorResponse)), security(("session_token"=[])))]
pub async fn get_usage_discount(
    Extension(repository): Extension<OrganizationUsageDiscountRepository>,
    Extension(_admin): Extension<AdminUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Option<UsageDiscountResponse>>, ApiError> {
    Ok(Json(
        repository
            .get(org_id)
            .await
            .map_err(api_error)?
            .map(Into::into),
    ))
}

/// Save one immutable discount. Repeating identical terms is idempotent. A different
/// second rule is rejected. Historical corrections run in bounded background batches;
/// consumers should refresh historical snapshots only after status becomes `active`.
#[utoipa::path(put, path="/v1/admin/organizations/{org_id}/usage-discount", tag="Admin",
    params(("org_id" = Uuid, Path, description="Organization ID")), request_body=UsageDiscountRequest,
    responses((status=200, description="Active discount", body=UsageDiscountResponse),
        (status=202, description="Historical correction in progress", body=UsageDiscountResponse),
        (status=400, description="Invalid percentage or date", body=ErrorResponse),
        (status=409, description="Conflicting rule or unsupported funding history", body=ErrorResponse),
        (status=401, description="Unauthorized", body=ErrorResponse)), security(("session_token"=[])))]
pub async fn put_usage_discount(
    Extension(repository): Extension<OrganizationUsageDiscountRepository>,
    Extension(admin): Extension<AdminUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<UsageDiscountRequest>,
) -> Result<(StatusCode, Json<UsageDiscountResponse>), ApiError> {
    let rule = repository
        .save(
            org_id,
            request.discount_basis_points,
            request.apply_since,
            admin.0.id,
        )
        .await
        .map_err(api_error)?;
    let status = if rule.status == "applying" {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(rule.into())))
}
