use chrono::{DateTime, Utc};
use std::{sync::Arc, time::Instant};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportingUsageSummarySource {
    All,
    Inference,
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportingUsageSummaryFilters {
    pub organization_id: Uuid,
    pub start_time: Option<DateTime<Utc>>,
    pub end_time: Option<DateTime<Utc>>,
    pub workspace_id: Option<Uuid>,
    pub api_key_id: Option<Uuid>,
    pub model: Option<String>,
    pub inference_type: Option<String>,
    pub service_name: Option<String>,
    pub source: ReportingUsageSummarySource,
    pub deadline: Option<Instant>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceUsageSummary {
    pub totals: InferenceUsageTotals,
    pub by_workspace: Vec<InferenceWorkspaceSummary>,
    pub by_api_key: Vec<InferenceApiKeySummary>,
    pub by_model: Vec<InferenceModelSummary>,
    pub by_day: Vec<InferenceDaySummary>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InferenceUsageTotals {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceWorkspaceSummary {
    pub workspace_id: Uuid,
    pub request_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceApiKeySummary {
    pub api_key_id: Uuid,
    pub request_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceModelSummary {
    pub model: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceDaySummary {
    pub day: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceUsageSummary {
    pub totals: ServiceUsageTotals,
    pub by_workspace: Vec<ServiceWorkspaceSummary>,
    pub by_api_key: Vec<ServiceApiKeySummary>,
    pub by_service: Vec<ServiceNameSummary>,
    pub by_day: Vec<ServiceDaySummary>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceUsageTotals {
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceWorkspaceSummary {
    pub workspace_id: Uuid,
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceApiKeySummary {
    pub api_key_id: Uuid,
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceNameSummary {
    pub service_name: String,
    pub usage_count: i64,
    pub quantity: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDaySummary {
    pub day: String,
    pub usage_count: i64,
    pub total_cost_nano_usd: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportingUsageSummary {
    pub inference: InferenceUsageSummary,
    pub service: ServiceUsageSummary,
}

#[derive(Debug, Error)]
pub enum ReportingUsageError {
    #[error("Usage reporting query timed out")]
    Timeout,
    #[error("Failed to summarize usage: {0}")]
    Internal(String),
}

#[async_trait::async_trait]
pub trait ReportingUsageSummaryRepository: Send + Sync {
    async fn summarize_usage(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> anyhow::Result<ReportingUsageSummary>;
}

pub struct ReportingUsageService {
    repository: Arc<dyn ReportingUsageSummaryRepository>,
}

impl ReportingUsageService {
    pub fn new(repository: Arc<dyn ReportingUsageSummaryRepository>) -> Self {
        Self { repository }
    }

    pub async fn summarize(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<ReportingUsageSummary, ReportingUsageError> {
        self.repository
            .summarize_usage(filters)
            .await
            .map_err(|error| {
                if crate::common::is_query_timeout(&error) {
                    ReportingUsageError::Timeout
                } else {
                    ReportingUsageError::Internal(error.to_string())
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    struct RecordingSummaryRepository {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ReportingUsageSummaryRepository for RecordingSummaryRepository {
        async fn summarize_usage(
            &self,
            filters: &ReportingUsageSummaryFilters,
        ) -> anyhow::Result<ReportingUsageSummary> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(filters.source, ReportingUsageSummarySource::All);
            Ok(ReportingUsageSummary::default())
        }
    }

    #[tokio::test]
    async fn reporting_usage_service_uses_one_composite_repository_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = ReportingUsageService::new(Arc::new(RecordingSummaryRepository {
            calls: calls.clone(),
        }));
        let filters = ReportingUsageSummaryFilters {
            organization_id: Uuid::new_v4(),
            start_time: None,
            end_time: None,
            workspace_id: None,
            api_key_id: None,
            model: None,
            inference_type: None,
            service_name: None,
            source: ReportingUsageSummarySource::All,
            deadline: None,
        };

        let summary = service.summarize(&filters).await.unwrap();

        assert_eq!(summary, ReportingUsageSummary::default());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
