use super::{
    ReportingUsageQuery, ReportingUsageQueryError, ReportingUsageQueryParams, ReportingUsageSource,
    ReportingUsageSummaryResponse, RouteError,
};
use crate::{middleware::AuthenticatedReportingToken, models::ErrorResponse};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use services::reporting_usage::{
    InferenceUsageSummary, InferenceUsageSummaryRepository, ReportingUsageSummaryFilters,
    ServiceUsageSummary, ServiceUsageSummaryRepository,
};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct ReportingUsageSummaryState {
    inference_repo: Arc<dyn InferenceUsageSummaryRepository>,
    service_repo: Arc<dyn ServiceUsageSummaryRepository>,
}

impl ReportingUsageSummaryState {
    pub fn new(
        inference_repo: Arc<dyn InferenceUsageSummaryRepository>,
        service_repo: Arc<dyn ServiceUsageSummaryRepository>,
    ) -> Self {
        Self {
            inference_repo,
            service_repo,
        }
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
        ("start_time" = Option<String>, Query, description = "Inclusive RFC3339 start timestamp. The range between start_time and end_time must not exceed 366 days."),
        ("end_time" = Option<String>, Query, description = "Inclusive RFC3339 end timestamp. Must be greater than or equal to start_time when both are provided."),
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
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("reporting_token" = [])
    )
)]
pub async fn summary_usage(
    State(state): State<ReportingUsageSummaryState>,
    Extension(reporting_token): Extension<AuthenticatedReportingToken>,
    Path(org_id): Path<Uuid>,
    Query(params): Query<ReportingUsageQueryParams>,
) -> Result<Json<ReportingUsageSummaryResponse>, RouteError> {
    ensure_token_matches_org(&reporting_token, org_id)?;
    let query = ReportingUsageQuery::try_from(params).map_err(query_error)?;
    let filters = summary_filters(org_id, &query);

    let inference = match query.source {
        ReportingUsageSource::All | ReportingUsageSource::Inference => state
            .inference_repo
            .summarize_inference_usage(&filters)
            .await
            .map_err(|_| internal_error("Failed to summarize inference usage"))?,
        ReportingUsageSource::Service => InferenceUsageSummary::default(),
    };
    let service = match query.source {
        ReportingUsageSource::All | ReportingUsageSource::Service => state
            .service_repo
            .summarize_service_usage(&filters)
            .await
            .map_err(|_| internal_error("Failed to summarize service usage"))?,
        ReportingUsageSource::Inference => ServiceUsageSummary::default(),
    };

    Ok(Json(super::summary_merge::summary_response(
        query, inference, service,
    )))
}

fn summary_filters(
    organization_id: Uuid,
    query: &ReportingUsageQuery,
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
    }
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

fn internal_error(message: &str) -> RouteError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            message.to_string(),
            "internal_server_error".to_string(),
        )),
    )
}
