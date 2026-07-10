use super::{
    ReportingInferenceUsage, ReportingServiceUsage, ReportingUsageExportRow,
    ReportingUsageRowSource,
};
use services::{service_usage::ports::ServiceUsageReportEntry, usage::InferenceUsageReportRow};
use uuid::Uuid;

pub(super) enum ExportRow {
    Inference(InferenceUsageReportRow),
    Service(ServiceUsageReportEntry),
}

impl ExportRow {
    pub(super) fn sort_key(&self) -> (chrono::DateTime<chrono::Utc>, u8, Uuid) {
        (self.created_at(), source_rank(self.source()), self.id())
    }

    fn created_at(&self) -> chrono::DateTime<chrono::Utc> {
        match self {
            Self::Inference(row) => row.created_at,
            Self::Service(row) => row.created_at,
        }
    }

    fn source(&self) -> ReportingUsageRowSource {
        match self {
            Self::Inference(_) => ReportingUsageRowSource::Inference,
            Self::Service(_) => ReportingUsageRowSource::Service,
        }
    }

    fn id(&self) -> Uuid {
        match self {
            Self::Inference(row) => row.id,
            Self::Service(row) => row.id,
        }
    }
}

impl From<ExportRow> for ReportingUsageExportRow {
    fn from(row: ExportRow) -> Self {
        match row {
            ExportRow::Inference(row) => inference_export_row(row),
            ExportRow::Service(row) => service_export_row(row),
        }
    }
}

fn inference_export_row(row: InferenceUsageReportRow) -> ReportingUsageExportRow {
    ReportingUsageExportRow {
        source: ReportingUsageRowSource::Inference,
        id: row.id,
        created_at: row.created_at,
        workspace_id: row.workspace_id,
        api_key_id: row.api_key_id,
        total_cost_nano_usd: row.total_cost_nano_usd,
        total_cost_usd: None,
        inference: Some(ReportingInferenceUsage {
            model: row.model,
            inference_type: row.inference_type,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_read_tokens: row.cache_read_tokens,
            total_tokens: row.total_tokens,
            input_cost_nano_usd: row.input_cost_nano_usd,
            output_cost_nano_usd: row.output_cost_nano_usd,
            cache_read_cost_nano_usd: row.cache_read_cost_nano_usd,
            total_cost_nano_usd: row.total_cost_nano_usd,
            response_id: row.response_id,
            inference_id: row.inference_id,
            image_count: row.image_count,
        }),
        service: None,
    }
}

fn service_export_row(row: ServiceUsageReportEntry) -> ReportingUsageExportRow {
    ReportingUsageExportRow {
        source: ReportingUsageRowSource::Service,
        id: row.id,
        created_at: row.created_at,
        workspace_id: row.workspace_id,
        api_key_id: row.api_key_id,
        total_cost_nano_usd: row.total_cost,
        total_cost_usd: None,
        inference: None,
        service: Some(ReportingServiceUsage {
            service_name: row.service_name,
            quantity: i64::from(row.quantity),
            total_cost_nano_usd: row.total_cost,
            inference_id: row.inference_id,
        }),
    }
}

pub(super) const fn source_rank(source: ReportingUsageRowSource) -> u8 {
    match source {
        ReportingUsageRowSource::Inference => 0,
        ReportingUsageRowSource::Service => 1,
    }
}
