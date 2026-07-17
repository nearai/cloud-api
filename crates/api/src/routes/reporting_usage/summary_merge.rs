use super::{
    ReportingApiKeySummary, ReportingDaySummary, ReportingModelSummary, ReportingServiceSummary,
    ReportingUsageQuery, ReportingUsageSummaryResponse, ReportingUsageTotals,
    ReportingWorkspaceSummary,
};
use chrono::{DateTime, Utc};
use services::reporting_usage::{InferenceUsageSummary, ServiceUsageSummary};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(super) fn summary_response(
    query: ReportingUsageQuery,
    inference: InferenceUsageSummary,
    service: ServiceUsageSummary,
) -> ReportingUsageSummaryResponse {
    ReportingUsageSummaryResponse {
        source: query.source,
        start_time: query.start_time.unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
        end_time: query.end_time.unwrap_or_else(Utc::now),
        totals: totals(&inference, &service),
        by_workspace: by_workspace(&inference, &service),
        by_api_key: by_api_key(&inference, &service),
        by_model: by_model(&inference),
        by_service: by_service(&service),
        by_day: by_day(&service, &inference),
    }
}

fn totals(
    inference: &InferenceUsageSummary,
    service: &ServiceUsageSummary,
) -> ReportingUsageTotals {
    let inference_cost = inference.totals.total_cost_nano_usd;
    let service_cost = service.totals.total_cost_nano_usd;
    ReportingUsageTotals {
        request_count: inference.totals.request_count,
        service_usage_count: service.totals.usage_count,
        input_tokens: inference.totals.input_tokens,
        output_tokens: inference.totals.output_tokens,
        cache_read_tokens: inference.totals.cache_read_tokens,
        total_tokens: inference.totals.total_tokens,
        inference_cost_nano_usd: inference_cost,
        service_cost_nano_usd: service_cost,
        total_cost_nano_usd: inference_cost + service_cost,
        total_cost_usd: None,
    }
}

fn by_workspace(
    inference: &InferenceUsageSummary,
    service: &ServiceUsageSummary,
) -> Vec<ReportingWorkspaceSummary> {
    let mut rows: BTreeMap<Uuid, ReportingWorkspaceSummary> = BTreeMap::new();
    for item in &inference.by_workspace {
        rows.insert(
            item.workspace_id,
            ReportingWorkspaceSummary {
                workspace_id: item.workspace_id,
                request_count: item.request_count,
                service_usage_count: 0,
                total_cost_nano_usd: item.total_cost_nano_usd,
            },
        );
    }
    for item in &service.by_workspace {
        let row = rows
            .entry(item.workspace_id)
            .or_insert(ReportingWorkspaceSummary {
                workspace_id: item.workspace_id,
                request_count: 0,
                service_usage_count: 0,
                total_cost_nano_usd: 0,
            });
        row.service_usage_count += item.usage_count;
        row.total_cost_nano_usd += item.total_cost_nano_usd;
    }
    sorted_workspace(rows)
}

fn by_api_key(
    inference: &InferenceUsageSummary,
    service: &ServiceUsageSummary,
) -> Vec<ReportingApiKeySummary> {
    let mut rows: BTreeMap<Uuid, ReportingApiKeySummary> = BTreeMap::new();
    for item in &inference.by_api_key {
        rows.insert(
            item.api_key_id,
            ReportingApiKeySummary {
                api_key_id: item.api_key_id,
                request_count: item.request_count,
                service_usage_count: 0,
                total_cost_nano_usd: item.total_cost_nano_usd,
            },
        );
    }
    for item in &service.by_api_key {
        let row = rows
            .entry(item.api_key_id)
            .or_insert(ReportingApiKeySummary {
                api_key_id: item.api_key_id,
                request_count: 0,
                service_usage_count: 0,
                total_cost_nano_usd: 0,
            });
        row.service_usage_count += item.usage_count;
        row.total_cost_nano_usd += item.total_cost_nano_usd;
    }
    sorted_api_key(rows)
}

fn by_model(inference: &InferenceUsageSummary) -> Vec<ReportingModelSummary> {
    inference
        .by_model
        .iter()
        .map(|item| ReportingModelSummary {
            model: item.model.clone(),
            request_count: item.request_count,
            input_tokens: item.input_tokens,
            output_tokens: item.output_tokens,
            cache_read_tokens: item.cache_read_tokens,
            total_tokens: item.total_tokens,
            total_cost_nano_usd: item.total_cost_nano_usd,
        })
        .collect()
}

fn by_service(service: &ServiceUsageSummary) -> Vec<ReportingServiceSummary> {
    service
        .by_service
        .iter()
        .map(|item| ReportingServiceSummary {
            service_name: item.service_name.clone(),
            usage_count: item.usage_count,
            quantity: item.quantity,
            total_cost_nano_usd: item.total_cost_nano_usd,
        })
        .collect()
}

fn by_day(
    service: &ServiceUsageSummary,
    inference: &InferenceUsageSummary,
) -> Vec<ReportingDaySummary> {
    let mut rows: BTreeMap<String, ReportingDaySummary> = BTreeMap::new();
    for item in &inference.by_day {
        rows.insert(
            item.day.clone(),
            ReportingDaySummary {
                day: item.day.clone(),
                request_count: item.request_count,
                service_usage_count: 0,
                input_tokens: item.input_tokens,
                output_tokens: item.output_tokens,
                cache_read_tokens: item.cache_read_tokens,
                total_tokens: item.total_tokens,
                inference_cost_nano_usd: item.total_cost_nano_usd,
                service_cost_nano_usd: 0,
                total_cost_nano_usd: item.total_cost_nano_usd,
            },
        );
    }
    for item in &service.by_day {
        let row = rows.entry(item.day.clone()).or_insert(ReportingDaySummary {
            day: item.day.clone(),
            request_count: 0,
            service_usage_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 0,
            inference_cost_nano_usd: 0,
            service_cost_nano_usd: 0,
            total_cost_nano_usd: 0,
        });
        row.service_usage_count += item.usage_count;
        row.service_cost_nano_usd += item.total_cost_nano_usd;
        row.total_cost_nano_usd += item.total_cost_nano_usd;
    }
    rows.into_values().collect()
}

fn sorted_workspace(
    rows: BTreeMap<Uuid, ReportingWorkspaceSummary>,
) -> Vec<ReportingWorkspaceSummary> {
    let mut values: Vec<_> = rows.into_values().collect();
    values.sort_by(|left, right| {
        right
            .total_cost_nano_usd
            .cmp(&left.total_cost_nano_usd)
            .then_with(|| left.workspace_id.cmp(&right.workspace_id))
    });
    values
}

fn sorted_api_key(rows: BTreeMap<Uuid, ReportingApiKeySummary>) -> Vec<ReportingApiKeySummary> {
    let mut values: Vec<_> = rows.into_values().collect();
    values.sort_by(|left, right| {
        right
            .total_cost_nano_usd
            .cmp(&left.total_cost_nano_usd)
            .then_with(|| left.api_key_id.cmp(&right.api_key_id))
    });
    values
}
