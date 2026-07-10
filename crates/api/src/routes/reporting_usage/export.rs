use super::{
    ensure_token_matches_org, export_rows::ExportRow, export_sources::list_rows, query_error,
    ReportingUsageCursor, ReportingUsageExportResponse, ReportingUsageExportRow,
    ReportingUsageQuery, ReportingUsageQueryError, ReportingUsageQueryParams,
    ReportingUsageRowSource, ReportingUsageSource, RouteError,
};
use crate::{
    middleware::{AuthenticatedReportingToken, ReportingRequestDeadline},
    models::ErrorResponse,
};
use axum::{
    extract::{Path, Query, State},
    Extension, Json,
};
use uuid::Uuid;

pub use super::export_sources::ReportingUsageExportState;

/// Export organization usage and cost rows.
///
/// Requires a reporting token scoped to the organization in the path. Results
/// are returned in descending `(created_at, source, id)` order with opaque
/// cursor pagination.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/usage/export",
    tag = "Reporting",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("start_time" = Option<String>, Query, description = "Inclusive RFC3339 start timestamp. Defaults to 366 days before the effective end_time."),
        ("end_time" = Option<String>, Query, description = "Inclusive RFC3339 end timestamp. Defaults to the request time. The effective range must not exceed 366 days."),
        ("source" = Option<ReportingUsageSource>, Query, description = "Usage source to export. Defaults to all."),
        ("workspace_id" = Option<Uuid>, Query, description = "Filter by workspace ID."),
        ("api_key_id" = Option<Uuid>, Query, description = "Filter by API key ID."),
        ("model" = Option<String>, Query, description = "Filter inference rows by model name."),
        ("inference_type" = Option<String>, Query, description = "Filter inference rows by inference type."),
        ("service_name" = Option<String>, Query, description = "Filter service rows by platform service name."),
        ("limit" = Option<u16>, Query, description = "Maximum rows to return. Defaults to 100 and must not exceed 1000."),
        ("cursor" = Option<String>, Query, description = "Opaque cursor returned by the previous export page.")
    ),
    responses(
        (status = 200, description = "Usage export page", body = ReportingUsageExportResponse),
        (status = 400, description = "Invalid filters or cursor", body = ErrorResponse),
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
pub async fn export_usage(
    State(state): State<ReportingUsageExportState>,
    Extension(reporting_token): Extension<AuthenticatedReportingToken>,
    Extension(request_deadline): Extension<ReportingRequestDeadline>,
    Path(org_id): Path<Uuid>,
    Query(params): Query<ReportingUsageQueryParams>,
) -> Result<Json<ReportingUsageExportResponse>, RouteError> {
    ensure_token_matches_org(&reporting_token, org_id)?;
    let query = ReportingUsageQuery::try_from(params).map_err(query_error)?;
    validate_cursor_organization(&query, org_id)?;
    validate_cursor_source(&query)?;

    let limit = usize::from(query.limit.value());
    let fetch_limit = query.limit.value() + 1;
    let rows = list_rows(&state, org_id, &query, fetch_limit, request_deadline).await?;

    Ok(Json(page_response(rows, limit, org_id, &query)?))
}

fn page_response(
    rows: Vec<ExportRow>,
    limit: usize,
    organization_id: Uuid,
    query: &ReportingUsageQuery,
) -> Result<ReportingUsageExportResponse, RouteError> {
    let has_more = rows.len() > limit;
    let data: Vec<ReportingUsageExportRow> = rows.into_iter().take(limit).map(Into::into).collect();
    let next_cursor = if has_more {
        data.last()
            .map(|row| {
                ReportingUsageCursor::for_query(
                    organization_id,
                    row.created_at,
                    row.source(),
                    row.id,
                    query,
                )
                .and_then(|cursor| cursor.encode())
            })
            .transpose()
            .map_err(query_error)?
    } else {
        None
    };
    Ok(ReportingUsageExportResponse { data, next_cursor })
}

fn validate_cursor_organization(
    query: &ReportingUsageQuery,
    organization_id: Uuid,
) -> Result<(), RouteError> {
    query
        .cursor
        .as_ref()
        .map(|cursor| cursor.validate_organization(organization_id))
        .transpose()
        .map_err(query_error)?;
    Ok(())
}

fn validate_cursor_source(query: &ReportingUsageQuery) -> Result<(), RouteError> {
    match (
        query.source,
        query.cursor.as_ref().map(|cursor| cursor.source),
    ) {
        (ReportingUsageSource::Inference, Some(ReportingUsageRowSource::Service))
        | (ReportingUsageSource::Service, Some(ReportingUsageRowSource::Inference)) => {
            Err(query_error(ReportingUsageQueryError::InvalidCursor))
        }
        _ => Ok(()),
    }
}
