use crate::repositories::{utils::map_db_error, OrganizationUsageRepository};
use crate::retry_db;
use anyhow::{Context, Result};
use services::{
    common::RepositoryError,
    reporting_usage::{
        InferenceApiKeySummary, InferenceDaySummary, InferenceModelSummary, InferenceUsageSummary,
        InferenceUsageSummaryRepository, InferenceUsageTotals, InferenceWorkspaceSummary,
        ReportingUsageSummaryFilters,
    },
};
use tokio_postgres::Row;

impl OrganizationUsageRepository {
    pub async fn summarize_inference_usage_report(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<InferenceUsageSummary> {
        let summary = retry_db!("summarize_inference_usage_report", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let totals = client
                .query_one(
                    r#"
                    SELECT COUNT(*)::bigint,
                           COALESCE(SUM(input_tokens), 0)::bigint,
                           COALESCE(SUM(output_tokens), 0)::bigint,
                           COALESCE(SUM(cache_read_tokens), 0)::bigint,
                           COALESCE(SUM(total_tokens), 0)::bigint,
                           COALESCE(SUM(total_cost), 0)::bigint
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_workspace = client
                .query(
                    r#"
                    SELECT workspace_id, COUNT(*)::bigint,
                           COALESCE(SUM(total_cost), 0)::bigint
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                    GROUP BY workspace_id
                    ORDER BY 3 DESC, workspace_id ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_api_key = client
                .query(
                    r#"
                    SELECT api_key_id, COUNT(*)::bigint,
                           COALESCE(SUM(total_cost), 0)::bigint
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                    GROUP BY api_key_id
                    ORDER BY 3 DESC, api_key_id ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_model = client
                .query(
                    r#"
                    SELECT model_name, COUNT(*)::bigint,
                           COALESCE(SUM(input_tokens), 0)::bigint,
                           COALESCE(SUM(output_tokens), 0)::bigint,
                           COALESCE(SUM(cache_read_tokens), 0)::bigint,
                           COALESCE(SUM(total_tokens), 0)::bigint,
                           COALESCE(SUM(total_cost), 0)::bigint
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                    GROUP BY model_name
                    ORDER BY 7 DESC, model_name ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            let by_day = client
                .query(
                    r#"
                    SELECT TO_CHAR(DATE_TRUNC('day', created_at), 'YYYY-MM-DD'),
                           COUNT(*)::bigint,
                           COALESCE(SUM(input_tokens), 0)::bigint,
                           COALESCE(SUM(output_tokens), 0)::bigint,
                           COALESCE(SUM(cache_read_tokens), 0)::bigint,
                           COALESCE(SUM(total_tokens), 0)::bigint,
                           COALESCE(SUM(total_cost), 0)::bigint
                    FROM organization_usage_log
                    WHERE organization_id = $1
                      AND ($2::TIMESTAMPTZ IS NULL OR created_at >= $2)
                      AND ($3::TIMESTAMPTZ IS NULL OR created_at <= $3)
                      AND ($4::UUID IS NULL OR workspace_id = $4)
                      AND ($5::UUID IS NULL OR api_key_id = $5)
                      AND ($6::TEXT IS NULL OR model_name = $6)
                      AND ($7::TEXT IS NULL OR inference_type = $7)
                    GROUP BY DATE_TRUNC('day', created_at)
                    ORDER BY 1 ASC
                    "#,
                    &[
                        &filters.organization_id,
                        &filters.start_time,
                        &filters.end_time,
                        &filters.workspace_id,
                        &filters.api_key_id,
                        &filters.model,
                        &filters.inference_type,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            Ok::<_, RepositoryError>(InferenceUsageSummary {
                totals: totals_from_row(&totals),
                by_workspace: by_workspace.iter().map(workspace_from_row).collect(),
                by_api_key: by_api_key.iter().map(api_key_from_row).collect(),
                by_model: by_model.iter().map(model_from_row).collect(),
                by_day: by_day.iter().map(day_from_row).collect(),
            })
        })?;

        Ok(summary)
    }
}

#[async_trait::async_trait]
impl InferenceUsageSummaryRepository for OrganizationUsageRepository {
    async fn summarize_inference_usage(
        &self,
        filters: &ReportingUsageSummaryFilters,
    ) -> Result<InferenceUsageSummary> {
        self.summarize_inference_usage_report(filters).await
    }
}

fn totals_from_row(row: &Row) -> InferenceUsageTotals {
    InferenceUsageTotals {
        request_count: row.get(0),
        input_tokens: row.get(1),
        output_tokens: row.get(2),
        cache_read_tokens: row.get(3),
        total_tokens: row.get(4),
        total_cost_nano_usd: row.get(5),
    }
}

fn workspace_from_row(row: &Row) -> InferenceWorkspaceSummary {
    InferenceWorkspaceSummary {
        workspace_id: row.get(0),
        request_count: row.get(1),
        total_cost_nano_usd: row.get(2),
    }
}

fn api_key_from_row(row: &Row) -> InferenceApiKeySummary {
    InferenceApiKeySummary {
        api_key_id: row.get(0),
        request_count: row.get(1),
        total_cost_nano_usd: row.get(2),
    }
}

fn model_from_row(row: &Row) -> InferenceModelSummary {
    InferenceModelSummary {
        model: row.get(0),
        request_count: row.get(1),
        input_tokens: row.get(2),
        output_tokens: row.get(3),
        cache_read_tokens: row.get(4),
        total_tokens: row.get(5),
        total_cost_nano_usd: row.get(6),
    }
}

fn day_from_row(row: &Row) -> InferenceDaySummary {
    InferenceDaySummary {
        day: row.get(0),
        request_count: row.get(1),
        input_tokens: row.get(2),
        output_tokens: row.get(3),
        cache_read_tokens: row.get(4),
        total_tokens: row.get(5),
        total_cost_nano_usd: row.get(6),
    }
}
