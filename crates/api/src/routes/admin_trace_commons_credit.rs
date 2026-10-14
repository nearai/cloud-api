//! Trace Commons contributor credit: set an organization's cumulative total.
//!
//! The billing analogue of the staking-farm sync: the caller states the
//! organization's cumulative Trace Commons credit, and the `trace_commons`
//! limit is raised to it, never lowered. Repeating a request, or delivering
//! an older total after a newer one, writes nothing.

use crate::{middleware::AdminUser, models::ErrorResponse};
use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    Json,
};
use database::repositories::trace_commons_credit::{
    TraceCommonsCreditError, TraceCommonsCreditRepository,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SetTraceCommonsCreditRequest {
    /// The organization's cumulative Trace Commons credit, in nano-USD
    /// (scale 9). A value at or below the current one is a no-op.
    #[serde(rename = "cumulativeNanoUsd")]
    pub cumulative_nano_usd: i64,
    /// An opaque reference to the caller's statement of this total, recorded
    /// as the change reason. 1-128 characters of `[A-Za-z0-9_.:-]`.
    #[serde(rename = "statementRef")]
    pub statement_ref: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct TraceCommonsCreditResponse {
    pub organization_id: String,
    /// The active `trace_commons` limit after the call, in nano-USD.
    #[serde(rename = "cumulativeNanoUsd")]
    pub cumulative_nano_usd: i64,
    /// Whether this call raised it. Always false for a GET.
    pub applied: bool,
}

type ApiError = (StatusCode, Json<ErrorResponse>);

fn api_error(error: anyhow::Error) -> ApiError {
    let (status, code, message) = match error.downcast_ref::<TraceCommonsCreditError>() {
        Some(TraceCommonsCreditError::Invalid(_)) => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            error.to_string(),
        ),
        Some(TraceCommonsCreditError::NotFound) => {
            (StatusCode::NOT_FOUND, "not_found", error.to_string())
        }
        Some(TraceCommonsCreditError::Conflict) => {
            (StatusCode::CONFLICT, "conflict", error.to_string())
        }
        None => {
            tracing::error!(error = %error, "Trace Commons credit write failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_server_error",
                "Failed to set Trace Commons credit".into(),
            )
        }
    };
    (status, Json(ErrorResponse::new(message, code.into())))
}

/// Raise an organization's Trace Commons credit to a cumulative total
/// (Admin only). Monotonic and idempotent.
#[utoipa::path(
    put,
    path = "/v1/admin/organizations/{org_id}/trace-commons/credit",
    tag = "Admin",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    request_body = SetTraceCommonsCreditRequest,
    responses(
        (status = 200, description = "Current cumulative credit, and whether this call raised it", body = TraceCommonsCreditResponse),
        (status = 400, description = "Invalid amount or statement reference", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Admin write permission required", body = ErrorResponse),
        (status = 404, description = "Organization not found or inactive", body = ErrorResponse),
        (status = 409, description = "Organization accounting is busy; retry", body = ErrorResponse),
    ),
    security(("session_token" = [])),
)]
pub async fn set_trace_commons_credit(
    Extension(repository): Extension<TraceCommonsCreditRepository>,
    Extension(admin): Extension<AdminUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<SetTraceCommonsCreditRequest>,
) -> Result<Json<TraceCommonsCreditResponse>, ApiError> {
    let write = repository
        .set_cumulative(
            org_id,
            request.cumulative_nano_usd,
            &request.statement_ref,
            Some(admin.0.id),
        )
        .await
        .map_err(api_error)?;
    Ok(Json(TraceCommonsCreditResponse {
        organization_id: org_id.to_string(),
        cumulative_nano_usd: write.cumulative_nano_usd,
        applied: write.applied,
    }))
}

/// Read an organization's cumulative Trace Commons credit (Admin only).
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/trace-commons/credit",
    tag = "Admin",
    params(("org_id" = Uuid, Path, description = "Organization ID")),
    responses(
        (status = 200, description = "Current cumulative credit (0 when none)", body = TraceCommonsCreditResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "Organization not found or inactive", body = ErrorResponse),
    ),
    security(("session_token" = [])),
)]
pub async fn get_trace_commons_credit(
    Extension(repository): Extension<TraceCommonsCreditRepository>,
    Extension(_admin): Extension<AdminUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<TraceCommonsCreditResponse>, ApiError> {
    let current = repository
        .get_cumulative(org_id)
        .await
        .map_err(api_error)?
        .ok_or_else(|| api_error(TraceCommonsCreditError::NotFound.into()))?;
    Ok(Json(TraceCommonsCreditResponse {
        organization_id: org_id.to_string(),
        cumulative_nano_usd: current,
        applied: false,
    }))
}
