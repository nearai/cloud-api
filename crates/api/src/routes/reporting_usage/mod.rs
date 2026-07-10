mod cursor;
pub mod export;
mod export_rows;
mod export_sources;
mod query;
mod schemas;
pub mod summary;
mod summary_merge;

use crate::{middleware::AuthenticatedReportingToken, models::ErrorResponse};
use axum::{extract::Path, http::StatusCode, Json};
use serde::Serialize;
use uuid::Uuid;

pub use cursor::ReportingUsageCursor;
pub use export::{export_usage, ReportingUsageExportState};
pub use query::{
    ReportingUsageLimit, ReportingUsageQuery, ReportingUsageQueryError, ReportingUsageQueryParams,
    ReportingUsageRowSource, ReportingUsageSource,
};
pub use schemas::{
    ReportingApiKeySummary, ReportingDaySummary, ReportingInferenceUsage, ReportingModelSummary,
    ReportingServiceSummary, ReportingServiceUsage, ReportingUsageDetails,
    ReportingUsageExportResponse, ReportingUsageExportRow, ReportingUsageSummaryResponse,
    ReportingUsageTotals, ReportingWorkspaceSummary,
};
pub use summary::{summary_usage, ReportingUsageSummaryState};

type RouteError = (StatusCode, Json<ErrorResponse>);

#[derive(Debug, Serialize)]
pub struct ReportingTokenAuthProbeResponse {
    pub token_id: Uuid,
    pub organization_id: Uuid,
    pub token_prefix: String,
    pub scope: services::reporting_tokens::ReportingTokenScope,
}

pub async fn reporting_token_auth_probe(
    axum::Extension(reporting_token): axum::Extension<AuthenticatedReportingToken>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<ReportingTokenAuthProbeResponse>, RouteError> {
    ensure_token_matches_org(&reporting_token, org_id)?;

    Ok(Json(ReportingTokenAuthProbeResponse {
        token_id: reporting_token.id,
        organization_id: reporting_token.organization_id,
        token_prefix: reporting_token.token_prefix,
        scope: reporting_token.scope,
    }))
}

fn ensure_token_matches_org(
    reporting_token: &AuthenticatedReportingToken,
    org_id: Uuid,
) -> Result<(), RouteError> {
    if reporting_token.organization_id == org_id {
        return Ok(());
    }
    Err((
        StatusCode::FORBIDDEN,
        Json(ErrorResponse::new(
            "Reporting token is not authorized for this organization.".to_string(),
            "forbidden".to_string(),
        )),
    ))
}

fn query_error(error: ReportingUsageQueryError) -> RouteError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new(
            error.to_string(),
            "invalid_reporting_usage_query".to_string(),
        )),
    )
}

fn timeout_error() -> RouteError {
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(ErrorResponse::new(
            "Usage reporting request timed out".to_string(),
            "reporting_request_timeout".to_string(),
        )),
    )
}

fn internal_error(message: &str) -> RouteError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            message.to_string(),
            "internal_server_error".to_string(),
        )),
    )
}

#[cfg(test)]
mod tests;
