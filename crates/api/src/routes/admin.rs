use crate::middleware::AdminUser;
use crate::models::ErrorResponse;
use axum::{extract::State, http::StatusCode, response::Json as ResponseJson, Extension};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tracing::debug;

/// Serialize a stored `deprecation_date` into the OpenRouter-compatible
/// UTC-hour form `YYYY-MM-DDTHH:00:00Z` (matching the spec's examples, e.g.
/// `2025-06-01T15:00:00Z`). The stored value is always normalized to a whole
/// UTC hour by [`parse_deprecation_date`], so this is a faithful round-trip.
pub(crate) fn format_deprecation_date(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:00:00Z").to_string()
}

#[derive(Clone)]
pub struct AdminAppState {
    pub infra_service: Arc<services::admin::InfraService>,
}

/// Get the platform infrastructure / fleet burn summary (Admin only)
///
/// Fetches the live host list, counts active/idle hosts, and computes the monthly/daily
/// GPU burn rate from the configured cost-per-host. Degrades gracefully (stale=true) if
/// the host inventory is unreachable.
#[utoipa::path(
    get,
    path = "/v1/admin/platform/infra-summary",
    tag = "Admin",
    responses(
        (status = 200, description = "Infra summary retrieved successfully", body = services::admin::InfraSummary),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_infra_summary(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<ResponseJson<services::admin::InfraSummary>, (StatusCode, ResponseJson<ErrorResponse>)>
{
    debug!("Get platform infra summary request");
    let summary = app_state.infra_service.get_infra_summary().await;
    Ok(ResponseJson(summary))
}
