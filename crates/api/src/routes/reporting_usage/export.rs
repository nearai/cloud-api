use super::{
    export_rows::{source_rank, ExportRow},
    ReportingUsageCursor, ReportingUsageExportResponse, ReportingUsageExportRow,
    ReportingUsageQuery, ReportingUsageQueryError, ReportingUsageQueryParams,
    ReportingUsageRowSource, ReportingUsageSource, RouteError,
};
use crate::{middleware::AuthenticatedReportingToken, models::ErrorResponse};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Extension, Json,
};
use services::{
    service_usage::{
        ports::{ServiceUsageReportCursor, ServiceUsageReportFilters},
        ServiceUsageServiceTrait,
    },
    usage::{InferenceUsageReportCursor, InferenceUsageReportQuery, UsageServiceTrait},
};
use std::cmp::Reverse;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct ReportingUsageExportState {
    usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    service_usage_service: Arc<dyn ServiceUsageServiceTrait + Send + Sync>,
}

impl ReportingUsageExportState {
    pub fn new(
        usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
        service_usage_service: Arc<dyn ServiceUsageServiceTrait + Send + Sync>,
    ) -> Self {
        Self {
            usage_service,
            service_usage_service,
        }
    }
}

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
        ("start_time" = Option<String>, Query, description = "Inclusive RFC3339 start timestamp. The range between start_time and end_time must not exceed 366 days."),
        ("end_time" = Option<String>, Query, description = "Inclusive RFC3339 end timestamp. Must be greater than or equal to start_time when both are provided."),
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
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("reporting_token" = [])
    )
)]
pub async fn export_usage(
    State(state): State<ReportingUsageExportState>,
    Extension(reporting_token): Extension<AuthenticatedReportingToken>,
    Path(org_id): Path<Uuid>,
    Query(params): Query<ReportingUsageQueryParams>,
) -> Result<Json<ReportingUsageExportResponse>, RouteError> {
    ensure_token_matches_org(&reporting_token, org_id)?;
    let query = ReportingUsageQuery::try_from(params).map_err(query_error)?;
    validate_cursor_source(&query)?;

    let limit = usize::from(query.limit.value());
    let fetch_limit = query.limit.value() + 1;
    let rows = match query.source {
        ReportingUsageSource::All => list_all_rows(&state, org_id, &query, fetch_limit).await?,
        ReportingUsageSource::Inference => {
            list_inference_rows(&state, org_id, &query, fetch_limit, query.cursor.clone()).await?
        }
        ReportingUsageSource::Service => {
            list_service_rows(&state, org_id, &query, fetch_limit, query.cursor.clone()).await?
        }
    };

    Ok(Json(page_response(rows, limit)?))
}

async fn list_all_rows(
    state: &ReportingUsageExportState,
    org_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
) -> Result<Vec<ExportRow>, RouteError> {
    let inference_cursor = source_cursor(query.cursor.as_ref(), ReportingUsageRowSource::Inference);
    let service_cursor = source_cursor(query.cursor.as_ref(), ReportingUsageRowSource::Service);
    let mut rows = list_inference_rows(state, org_id, query, fetch_limit, inference_cursor).await?;
    rows.extend(list_service_rows(state, org_id, query, fetch_limit, service_cursor).await?);
    rows.sort_by_key(|row| Reverse(row.sort_key()));
    rows.truncate(usize::from(fetch_limit));
    Ok(rows)
}

async fn list_inference_rows(
    state: &ReportingUsageExportState,
    org_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
    cursor: Option<ReportingUsageCursor>,
) -> Result<Vec<ExportRow>, RouteError> {
    let rows = state
        .usage_service
        .list_inference_usage_report(InferenceUsageReportQuery {
            organization_id: org_id,
            start_time: query.start_time,
            end_time: query.end_time,
            workspace_id: query.workspace_id,
            api_key_id: query.api_key_id,
            model: query.model.clone(),
            inference_type: query.inference_type.map(|value| value.as_str().to_string()),
            limit: fetch_limit,
            cursor: cursor.map(|value| InferenceUsageReportCursor {
                created_at: value.created_at,
                id: value.id,
            }),
        })
        .await
        .map_err(|_| internal_error("Failed to list inference usage export"))?;

    Ok(rows.into_iter().map(ExportRow::Inference).collect())
}

async fn list_service_rows(
    state: &ReportingUsageExportState,
    org_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
    cursor: Option<ReportingUsageCursor>,
) -> Result<Vec<ExportRow>, RouteError> {
    let rows = state
        .service_usage_service
        .get_usage_report(&ServiceUsageReportFilters {
            organization_id: org_id,
            service_name: query.service_name.clone(),
            workspace_id: query.workspace_id,
            api_key_id: query.api_key_id,
            start_time: query.start_time,
            end_time: query.end_time,
            cursor: cursor.map(|value| ServiceUsageReportCursor {
                created_at: value.created_at,
                id: value.id,
            }),
            limit: i64::from(fetch_limit),
        })
        .await
        .map_err(|_| internal_error("Failed to list service usage export"))?;

    Ok(rows.into_iter().map(ExportRow::Service).collect())
}

fn page_response(
    rows: Vec<ExportRow>,
    limit: usize,
) -> Result<ReportingUsageExportResponse, RouteError> {
    let has_more = rows.len() > limit;
    let data: Vec<ReportingUsageExportRow> = rows.into_iter().take(limit).map(Into::into).collect();
    let next_cursor = if has_more {
        data.last()
            .map(|row| ReportingUsageCursor::new(row.created_at, row.source, row.id).encode())
            .transpose()
            .map_err(query_error)?
    } else {
        None
    };
    Ok(ReportingUsageExportResponse { data, next_cursor })
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

fn source_cursor(
    cursor: Option<&ReportingUsageCursor>,
    row_source: ReportingUsageRowSource,
) -> Option<ReportingUsageCursor> {
    let cursor = cursor?;
    let row_rank = source_rank(row_source);
    let cursor_rank = source_rank(cursor.source);
    let id = match row_rank.cmp(&cursor_rank) {
        std::cmp::Ordering::Greater => Uuid::nil(),
        std::cmp::Ordering::Equal => cursor.id,
        std::cmp::Ordering::Less => Uuid::from_u128(u128::MAX),
    };
    Some(ReportingUsageCursor::new(cursor.created_at, row_source, id))
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
