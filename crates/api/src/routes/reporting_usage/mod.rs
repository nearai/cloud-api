pub mod export;
mod export_rows;
mod query;
mod schemas;
pub mod summary;
mod summary_merge;

use crate::{middleware::AuthenticatedReportingToken, models::ErrorResponse};
use axum::{extract::Path, http::StatusCode, Json};
use serde::Serialize;
use uuid::Uuid;

pub use export::{export_usage, ReportingUsageExportState};
pub use query::{
    ReportingUsageCursor, ReportingUsageLimit, ReportingUsageQuery, ReportingUsageQueryError,
    ReportingUsageQueryParams, ReportingUsageRowSource, ReportingUsageSource,
};
pub use schemas::{
    ReportingApiKeySummary, ReportingDaySummary, ReportingInferenceUsage, ReportingModelSummary,
    ReportingServiceSummary, ReportingServiceUsage, ReportingUsageExportResponse,
    ReportingUsageExportRow, ReportingUsageSummaryResponse, ReportingUsageTotals,
    ReportingWorkspaceSummary,
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
    if reporting_token.organization_id != org_id {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Reporting token is not authorized for this organization.".to_string(),
                "forbidden".to_string(),
            )),
        ));
    }

    Ok(Json(ReportingTokenAuthProbeResponse {
        token_id: reporting_token.id,
        organization_id: reporting_token.organization_id,
        token_prefix: reporting_token.token_prefix,
        scope: reporting_token.scope,
    }))
}

#[cfg(test)]
mod tests;
