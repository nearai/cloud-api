use super::{
    export_rows::{source_rank, ExportRow},
    internal_error, timeout_error, ReportingUsageCursor, ReportingUsageQuery,
    ReportingUsageRowSource, ReportingUsageSource, RouteError,
};
use crate::middleware::ReportingRequestDeadline;
use services::{
    service_usage::{
        ports::{ServiceUsageReportCursor, ServiceUsageReportFilters},
        ServiceUsageServiceTrait,
    },
    usage::{InferenceUsageReportCursor, InferenceUsageReportQuery, UsageServiceTrait},
};
use std::{cmp::Reverse, sync::Arc};
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

pub async fn list_rows(
    state: &ReportingUsageExportState,
    organization_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
    request_deadline: ReportingRequestDeadline,
) -> Result<Vec<ExportRow>, RouteError> {
    match query.source {
        ReportingUsageSource::All => {
            list_all_rows(state, organization_id, query, fetch_limit, request_deadline).await
        }
        ReportingUsageSource::Inference => {
            list_inference_rows(
                state,
                organization_id,
                query,
                fetch_limit,
                query.cursor.clone(),
                request_deadline,
            )
            .await
        }
        ReportingUsageSource::Service => {
            list_service_rows(
                state,
                organization_id,
                query,
                fetch_limit,
                query.cursor.clone(),
                request_deadline,
            )
            .await
        }
    }
}

async fn list_all_rows(
    state: &ReportingUsageExportState,
    organization_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
    request_deadline: ReportingRequestDeadline,
) -> Result<Vec<ExportRow>, RouteError> {
    let inference_cursor = source_cursor(query.cursor.as_ref(), ReportingUsageRowSource::Inference);
    let service_cursor = source_cursor(query.cursor.as_ref(), ReportingUsageRowSource::Service);
    let (mut rows, service_rows) = tokio::try_join!(
        list_inference_rows(
            state,
            organization_id,
            query,
            fetch_limit,
            inference_cursor,
            request_deadline,
        ),
        list_service_rows(
            state,
            organization_id,
            query,
            fetch_limit,
            service_cursor,
            request_deadline,
        ),
    )?;
    rows.extend(service_rows);
    rows.sort_by_key(|row| Reverse(row.sort_key()));
    rows.truncate(usize::from(fetch_limit));
    Ok(rows)
}

async fn list_inference_rows(
    state: &ReportingUsageExportState,
    organization_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
    cursor: Option<ReportingUsageCursor>,
    request_deadline: ReportingRequestDeadline,
) -> Result<Vec<ExportRow>, RouteError> {
    let rows = state
        .usage_service
        .list_inference_usage_report(InferenceUsageReportQuery {
            organization_id,
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
            deadline: Some(request_deadline.instant()),
        })
        .await
        .map_err(|error| match error {
            services::usage::UsageError::ReportingTimeout => timeout_error(),
            _ => internal_error("Failed to list inference usage export"),
        })?;
    Ok(rows.into_iter().map(ExportRow::Inference).collect())
}

async fn list_service_rows(
    state: &ReportingUsageExportState,
    organization_id: Uuid,
    query: &ReportingUsageQuery,
    fetch_limit: u16,
    cursor: Option<ReportingUsageCursor>,
    request_deadline: ReportingRequestDeadline,
) -> Result<Vec<ExportRow>, RouteError> {
    let rows = state
        .service_usage_service
        .get_usage_report(&ServiceUsageReportFilters {
            organization_id,
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
            deadline: Some(request_deadline.instant()),
        })
        .await
        .map_err(|error| match error {
            services::service_usage::ServiceUsageError::ReportingTimeout => timeout_error(),
            _ => internal_error("Failed to list service usage export"),
        })?;
    Ok(rows.into_iter().map(ExportRow::Service).collect())
}

fn source_cursor(
    cursor: Option<&ReportingUsageCursor>,
    row_source: ReportingUsageRowSource,
) -> Option<ReportingUsageCursor> {
    let cursor = cursor?;
    let id = match source_rank(row_source).cmp(&source_rank(cursor.source)) {
        std::cmp::Ordering::Greater => Uuid::nil(),
        std::cmp::Ordering::Equal => cursor.id,
        std::cmp::Ordering::Less => Uuid::from_u128(u128::MAX),
    };
    Some(cursor.with_position(row_source, id))
}
