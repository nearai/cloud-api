use crate::middleware::AdminUser;
use crate::models::{CreditType, ErrorResponse};
use crate::routes::common::analytics_error_response;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use chrono::{DateTime, Utc};
use services::admin::AnalyticsService;
use std::sync::Arc;
use tracing::debug;

/// Serialize a stored `deprecation_date` into the OpenRouter-compatible
/// UTC-hour form `YYYY-MM-DDTHH:00:00Z` (matching the spec's examples, e.g.
/// `2025-06-01T15:00:00Z`). The stored value is always normalized to a whole
/// UTC hour, so this is a faithful round-trip.
pub(crate) fn format_deprecation_date(dt: &DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:00:00Z").to_string()
}

#[derive(Clone)]
pub struct AdminAppState {
    pub analytics_service: Arc<AnalyticsService>,
    pub infra_service: Arc<services::admin::InfraService>,
}

#[derive(Debug, serde::Deserialize)]
pub struct MetricsQueryParams {
    /// Start of time range (ISO 8601 format). Defaults to 30 days ago.
    pub start: Option<String>,
    /// End of time range (ISO 8601 format). Defaults to now.
    pub end: Option<String>,
}

const CREDIT_TYPE_QUERY_DESCRIPTION: &str = "Filter consumed inference usage by grant, staking_farm, transfer, payment, or postpay. Costs include only saved matching allocations, including settlements. Each matching request and its full tokens count once; counts are not additive across credit types. Unattributed historical usage is excluded. Period totals may increase until outstanding unfunded usage is settled. Omit for all usage.";

#[derive(Debug, serde::Deserialize)]
pub struct OrganizationMetricsQueryParams {
    #[serde(flatten)]
    pub metrics: MetricsQueryParams,
    /// Optional saved credit allocation type: grant, staking_farm, transfer, payment, or postpay.
    pub credit_type: Option<String>,
}

fn parse_metrics_credit_type(
    value: Option<&str>,
) -> Result<Option<String>, (StatusCode, ResponseJson<ErrorResponse>)> {
    value
        .map(|value| {
            value
                .parse::<CreditType>()
                .map(|kind| kind.as_str().to_string())
                .map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        ResponseJson(ErrorResponse::new(
                            format!("invalid credit_type: {value}"),
                            "invalid_request".to_string(),
                        )),
                    )
                })
        })
        .transpose()
}

/// Get organization metrics (Admin only)
///
/// Returns usage metrics for an organization including summary totals,
/// and breakdowns by workspace, API key, and model.
///
/// Percentiles across hours are approximate (sample-weighted means of hourly percentiles).
/// TTFT threshold counts are exact; divide them by `ttft_measured_requests`, not `requests` / `total_requests`.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/metrics",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID to get metrics for"),
        ("start" = Option<String>, Query, description = "Start of time range (ISO 8601). Defaults to 30 days ago. Window may not exceed 366 days."),
        ("end" = Option<String>, Query, description = "End of time range (ISO 8601). Defaults to now. Window may not exceed 366 days."),
        ("credit_type" = Option<CreditType>, Query, description = CREDIT_TYPE_QUERY_DESCRIPTION)
    ),
    responses(
        (status = 200, description = "Organization metrics retrieved successfully", body = services::admin::OrganizationMetrics),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 504, description = "Analytics statement budget exceeded", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_metrics(
    State(app_state): State<AdminAppState>,
    Path(org_id): Path<String>,
    Query(params): Query<OrganizationMetricsQueryParams>,
    Extension(_admin_user): Extension<AdminUser>,
) -> Result<
    ResponseJson<services::admin::OrganizationMetrics>,
    (StatusCode, ResponseJson<ErrorResponse>),
> {
    let credit_type = parse_metrics_credit_type(params.credit_type.as_deref())?;

    debug!(
        credit_type = ?credit_type,
        "Get organization metrics request for org_id: {}, start: {:?}, end: {:?}",
        org_id, params.metrics.start, params.metrics.end
    );

    // Parse organization ID
    let organization_id = uuid::Uuid::parse_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                "Invalid organization ID format".to_string(),
                "invalid_id".to_string(),
            )),
        )
    })?;

    let (start, end) = crate::routes::common::parse_metrics_range(
        params.metrics.start.as_deref(),
        params.metrics.end.as_deref(),
        None,
        0,
        366,
    )?;

    let metrics = app_state
        .analytics_service
        .get_organization_metrics(organization_id, start, end, credit_type.as_deref())
        .await
        .map_err(|e| analytics_error_response(e, "Failed to retrieve metrics", true))?;

    Ok(ResponseJson(metrics))
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
