//! Admin repair for the usage_hourly aggregate: recompute an explicit UTC window when raw
//! rows landed after the scheduler's 3-hour re-read (backfill, clock skew, a parity warning).

use crate::middleware::AdminUser;
use crate::models::ErrorResponse;
use axum::{extract::Extension, http::StatusCode, response::Json as ResponseJson, Json};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use services::usage::ports::UsageHourlyRepository;
use std::sync::Arc;
use utoipa::ToSchema;

#[derive(Clone)]
pub struct UsageHourlyRepairState {
    pub repository: Arc<dyn UsageHourlyRepository>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UsageHourlyRepairRequest {
    /// Inclusive start; a whole UTC hour.
    pub start: DateTime<Utc>,
    /// Exclusive end; a whole UTC hour, at most 31 days after `start`.
    pub end: DateTime<Utc>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UsageHourlyDayParity {
    pub day: NaiveDate,
    /// True when raw and aggregate totals for the whole UTC day match.
    pub ok: bool,
    pub raw_requests: i64,
    pub aggregate_requests: i64,
    pub raw_total_cost_nano: i64,
    pub aggregate_total_cost_nano: i64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UsageHourlyRepairResponse {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub rows_written: u64,
    pub days: Vec<UsageHourlyDayParity>,
}

/// Recompute usage_hourly for a UTC window (Admin only)
///
/// Rebuilds `[start, end)` from raw usage one UTC day per transaction, then reports raw vs
/// aggregate parity for every day the window touches. Use it after a backfill or when the
/// nightly parity check warns; the scheduler only re-reads the last 3 hours.
#[utoipa::path(
    post,
    path = "/v1/admin/usage-hourly/recompute",
    tag = "Admin",
    request_body = UsageHourlyRepairRequest,
    responses(
        (status = 200, description = "Window recomputed", body = UsageHourlyRepairResponse),
        (status = 400, description = "Invalid window", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(("session_token" = []))
)]
pub async fn recompute_usage_hourly(
    Extension(state): Extension<UsageHourlyRepairState>,
    Extension(_admin_user): Extension<AdminUser>,
    Json(request): Json<UsageHourlyRepairRequest>,
) -> Result<ResponseJson<UsageHourlyRepairResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    services::usage::validate_repair_window(request.start, request.end).map_err(|message| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(message, "invalid_request".to_string())),
        )
    })?;
    let report = services::usage::repair(state.repository.as_ref(), request.start, request.end)
        .await
        .map_err(|error| {
            tracing::error!(error = %error, "usage_hourly repair failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to recompute usage_hourly".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;
    Ok(ResponseJson(UsageHourlyRepairResponse {
        start: request.start,
        end: request.end,
        rows_written: report.rows_written,
        days: report
            .days
            .iter()
            .map(|parity| UsageHourlyDayParity {
                day: parity.day,
                ok: parity.is_ok(),
                raw_requests: parity.raw.request_count,
                aggregate_requests: parity.aggregate.request_count,
                raw_total_cost_nano: parity.raw.total_cost,
                aggregate_total_cost_nano: parity.aggregate.total_cost,
            })
            .collect(),
    }))
}
