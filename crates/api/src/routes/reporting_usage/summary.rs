use super::{
    ensure_token_matches_org, internal_error, query_error, timeout_error, ReportingUsageQuery,
    ReportingUsageQueryParams, ReportingUsageSource, ReportingUsageSummaryResponse, RouteError,
};
use crate::{
    middleware::{AuthenticatedReportingToken, ReportingRequestDeadline},
    models::ErrorResponse,
};
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use services::reporting_usage::{
    ReportingUsageError, ReportingUsageService, ReportingUsageSummaryFilters,
    ReportingUsageSummarySource,
};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct ReportingUsageSummaryState {
    service: Arc<ReportingUsageService>,
}

impl ReportingUsageSummaryState {
    pub fn new(service: Arc<ReportingUsageService>) -> Self {
        Self { service }
    }
}

/// Summarize organization usage and costs.
///
/// Requires a reporting token scoped to the organization in the path. Totals
/// include inference and platform service usage by default.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/summary",
    tag = "Reporting",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("start_time" = Option<String>, Query, description = "Inclusive RFC3339 start timestamp. Defaults to 366 days before the effective end_time."),
        ("end_time" = Option<String>, Query, description = "Inclusive RFC3339 end timestamp. Defaults to the request time. The effective range must not exceed 366 days."),
        ("source" = Option<ReportingUsageSource>, Query, description = "Usage source to summarize. Defaults to all."),
        ("workspace_id" = Option<Uuid>, Query, description = "Filter by workspace ID."),
        ("api_key_id" = Option<Uuid>, Query, description = "Filter by API key ID."),
        ("model" = Option<String>, Query, description = "Filter inference usage by model name."),
        ("inference_type" = Option<String>, Query, description = "Filter inference usage by inference type."),
        ("service_name" = Option<String>, Query, description = "Filter service usage by platform service name.")
    ),
    responses(
        (status = 200, description = "Usage summary", body = ReportingUsageSummaryResponse),
        (status = 400, description = "Invalid filters", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Reporting token is not scoped to this organization", body = ErrorResponse),
        (status = 429, description = "Reporting rate or concurrency limit exceeded", body = ErrorResponse),
        (status = 504, description = "Reporting request timed out", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("reporting_token" = [])
    )
)]
pub async fn summary_usage(
    State(state): State<ReportingUsageSummaryState>,
    Extension(reporting_token): Extension<AuthenticatedReportingToken>,
    Extension(request_deadline): Extension<ReportingRequestDeadline>,
    Path(org_id): Path<Uuid>,
    Query(params): Query<ReportingUsageQueryParams>,
) -> Result<Json<ReportingUsageSummaryResponse>, RouteError> {
    ensure_token_matches_org(&reporting_token, org_id)?;
    let query = ReportingUsageQuery::try_from(params).map_err(query_error)?;
    query
        .cursor
        .as_ref()
        .map(|cursor| cursor.validate_organization(org_id))
        .transpose()
        .map_err(query_error)?;
    let filters = summary_filters(org_id, &query, request_deadline);
    let summary = state
        .service
        .summarize(&filters)
        .await
        .map_err(|error| match error {
            ReportingUsageError::Timeout => timeout_error(),
            ReportingUsageError::Internal(_) => internal_error("Failed to summarize usage"),
        })?;

    Ok(Json(super::summary_merge::summary_response(
        query,
        summary.inference,
        summary.service,
    )))
}

fn summary_filters(
    organization_id: Uuid,
    query: &ReportingUsageQuery,
    request_deadline: ReportingRequestDeadline,
) -> ReportingUsageSummaryFilters {
    ReportingUsageSummaryFilters {
        organization_id,
        start_time: query.start_time,
        end_time: query.end_time,
        workspace_id: query.workspace_id,
        api_key_id: query.api_key_id,
        model: query.model.clone(),
        inference_type: query.inference_type.map(|value| value.as_str().to_string()),
        service_name: query.service_name.clone(),
        source: match query.source {
            ReportingUsageSource::All => ReportingUsageSummarySource::All,
            ReportingUsageSource::Inference => ReportingUsageSummarySource::Inference,
            ReportingUsageSource::Service => ReportingUsageSummarySource::Service,
        },
        deadline: Some(request_deadline.instant()),
    }
}
