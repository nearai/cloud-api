//! Analytics repository implementation for enterprise dashboard queries.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency.

use crate::pool::DbPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::admin::{
    AnalyticsRepository, ApiKeyMetrics, MetricsSummary, ModelMetrics, OrganizationMetrics,
    PlatformMetrics, TimeSeriesMetrics, TimeSeriesPoint, TopModelMetrics, TopOrganizationMetrics,
    WorkspaceMetrics,
};
use services::common::RepositoryError;
use uuid::Uuid;

/// PostgreSQL implementation of the analytics repository
pub struct PgAnalyticsRepository {
    pool: DbPool,
}

impl PgAnalyticsRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// Convert nano-dollars (scale 9) to USD
fn nano_to_usd(nano: i64) -> f64 {
    nano as f64 / 1_000_000_000.0
}

#[async_trait]
impl AnalyticsRepository for PgAnalyticsRepository {
    async fn get_organization_metrics(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<OrganizationMetrics, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // Get organization name
        let org_row = client
            .query_one("SELECT name FROM organizations WHERE id = $1", &[&org_id])
            .await
            .map_err(|_| RepositoryError::NotFound(format!("Organization {org_id}")))?;
        let org_name: String = org_row.get(0);

        // Get summary metrics including unique API keys
        let summary_row = client
            .query_one(
                r#"
                SELECT 
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                    COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                    COALESCE(SUM(total_cost), 0)::bigint as cost_nano,
                    COUNT(DISTINCT api_key_id)::bigint as unique_api_keys
                FROM organization_usage_log
                WHERE organization_id = $1
                  AND created_at >= $2
                  AND created_at < $3
                "#,
                &[&org_id, &start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let summary = MetricsSummary {
            total_requests: summary_row.get::<_, i64>(0),
            total_input_tokens: summary_row.get::<_, i64>(1),
            total_output_tokens: summary_row.get::<_, i64>(2),
            total_cost_usd: nano_to_usd(summary_row.get::<_, i64>(3)),
            unique_api_keys: summary_row.get::<_, i64>(4),
        };

        // Get metrics by workspace
        let workspace_rows = client
            .query(
                r#"
                SELECT 
                    w.id as workspace_id,
                    w.name as workspace_name,
                    COUNT(ul.id)::bigint as requests,
                    COALESCE(SUM(ul.input_tokens), 0)::bigint as input_tokens,
                    COALESCE(SUM(ul.output_tokens), 0)::bigint as output_tokens,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as cost_nano
                FROM workspaces w
                LEFT JOIN organization_usage_log ul ON ul.workspace_id = w.id 
                    AND ul.created_at >= $2 
                    AND ul.created_at < $3
                WHERE w.organization_id = $1
                GROUP BY w.id, w.name
                ORDER BY requests DESC
                "#,
                &[&org_id, &start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let by_workspace: Vec<WorkspaceMetrics> = workspace_rows
            .iter()
            .map(|row| WorkspaceMetrics {
                workspace_id: row.get(0),
                workspace_name: row.get(1),
                requests: row.get(2),
                input_tokens: row.get(3),
                output_tokens: row.get(4),
                cost_usd: nano_to_usd(row.get::<_, i64>(5)),
            })
            .collect();

        // Get metrics by API key
        let api_key_rows = client
            .query(
                r#"
                SELECT 
                    ak.id as api_key_id,
                    ak.name as api_key_name,
                    COUNT(ul.id)::bigint as requests,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as cost_nano
                FROM api_keys ak
                LEFT JOIN organization_usage_log ul ON ul.api_key_id = ak.id 
                    AND ul.created_at >= $2 
                    AND ul.created_at < $3
                WHERE ak.workspace_id IN (
                    SELECT id FROM workspaces WHERE organization_id = $1
                )
                GROUP BY ak.id, ak.name
                ORDER BY requests DESC
                "#,
                &[&org_id, &start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let by_api_key: Vec<ApiKeyMetrics> = api_key_rows
            .iter()
            .map(|row| ApiKeyMetrics {
                api_key_id: row.get(0),
                api_key_name: row.get(1),
                requests: row.get(2),
                cost_usd: nano_to_usd(row.get::<_, i64>(3)),
            })
            .collect();

        // Get metrics by model (including latency metrics: TTFT and ITL)
        let model_rows = client
            .query(
                r#"
                SELECT 
                    ul.model_name,
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as cost_nano,
                    AVG(ul.ttft_ms)::double precision as avg_ttft_ms,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms,
                    AVG(ul.avg_itl_ms)::double precision as avg_itl_ms,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.avg_itl_ms)::double precision as p95_itl_ms
                FROM organization_usage_log ul
                WHERE ul.organization_id = $1
                  AND ul.created_at >= $2
                  AND ul.created_at < $3
                GROUP BY ul.model_name
                ORDER BY requests DESC
                "#,
                &[&org_id, &start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let by_model: Vec<ModelMetrics> = model_rows
            .iter()
            .map(|row| ModelMetrics {
                model_name: row.get(0),
                requests: row.get(1),
                avg_ttft_ms: row.get::<_, Option<f64>>(3),
                p95_ttft_ms: row.get::<_, Option<f64>>(4),
                avg_itl_ms: row.get::<_, Option<f64>>(5),
                p95_itl_ms: row.get::<_, Option<f64>>(6),
                cost_usd: nano_to_usd(row.get::<_, i64>(2)),
            })
            .collect();

        Ok(OrganizationMetrics {
            organization_id: org_id,
            organization_name: org_name,
            period_start: start,
            period_end: end,
            summary,
            by_workspace,
            by_api_key,
            by_model,
        })
    }

    async fn get_platform_metrics(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<PlatformMetrics, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // Get total users and organizations
        let counts_row = client
            .query_one(
                r#"
                SELECT 
                    (SELECT COUNT(*) FROM users WHERE is_active = true)::bigint as total_users,
                    (SELECT COUNT(*) FROM organizations WHERE is_active = true)::bigint as total_organizations
                "#,
                &[],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let total_users: i64 = counts_row.get(0);
        let total_organizations: i64 = counts_row.get(1);

        // Get usage summary across all organizations
        let summary_row = client
            .query_one(
                r#"
                SELECT 
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(total_cost), 0)::bigint as revenue_nano
                FROM organization_usage_log
                WHERE created_at >= $1 AND created_at < $2
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let total_requests: i64 = summary_row.get(0);
        let total_revenue_usd = nano_to_usd(summary_row.get::<_, i64>(1));

        // Get top 10 models by request count
        let top_models_rows = client
            .query(
                r#"
                SELECT 
                    model_name,
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(total_cost), 0)::bigint as revenue_nano
                FROM organization_usage_log
                WHERE created_at >= $1 AND created_at < $2
                GROUP BY model_name
                ORDER BY requests DESC
                LIMIT 10
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let top_models: Vec<TopModelMetrics> = top_models_rows
            .iter()
            .map(|row| TopModelMetrics {
                model_name: row.get(0),
                requests: row.get(1),
                revenue_usd: nano_to_usd(row.get::<_, i64>(2)),
            })
            .collect();

        // Get top 10 organizations by spend
        let top_orgs_rows = client
            .query(
                r#"
                SELECT 
                    o.id as organization_id,
                    o.name as organization_name,
                    COUNT(ul.id)::bigint as requests,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as spend_nano
                FROM organizations o
                INNER JOIN organization_usage_log ul ON ul.organization_id = o.id
                WHERE ul.created_at >= $1 AND ul.created_at < $2
                GROUP BY o.id, o.name
                ORDER BY spend_nano DESC
                LIMIT 10
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let top_organizations: Vec<TopOrganizationMetrics> = top_orgs_rows
            .iter()
            .map(|row| TopOrganizationMetrics {
                organization_id: row.get(0),
                organization_name: row.get(1),
                requests: row.get(2),
                spend_usd: nano_to_usd(row.get::<_, i64>(3)),
            })
            .collect();

        Ok(PlatformMetrics {
            period_start: start,
            period_end: end,
            total_users,
            total_organizations,
            total_requests,
            total_revenue_usd,
            top_models,
            top_organizations,
        })
    }

    async fn get_organization_timeseries(
        &self,
        org_id: Uuid,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<TimeSeriesMetrics, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // Get organization name
        let org_row = client
            .query_one("SELECT name FROM organizations WHERE id = $1", &[&org_id])
            .await
            .map_err(|_| RepositoryError::NotFound(format!("Organization {org_id}")))?;
        let org_name: String = org_row.get(0);

        // Determine date truncation based on granularity
        let date_trunc = match granularity {
            "hour" => "hour",
            "week" => "week",
            _ => "day", // default to day
        };

        // Get time series data
        let query = format!(
            r#"
            SELECT 
                DATE_TRUNC('{}', created_at)::text as date,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM organization_usage_log
            WHERE organization_id = $1
              AND created_at >= $2
              AND created_at < $3
            GROUP BY DATE_TRUNC('{}', created_at)
            ORDER BY date ASC
            "#,
            date_trunc, date_trunc
        );

        let rows = client
            .query(&query, &[&org_id, &start, &end])
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let data: Vec<TimeSeriesPoint> = rows
            .iter()
            .map(|row| TimeSeriesPoint {
                date: row.get(0),
                requests: row.get(1),
                input_tokens: row.get(2),
                output_tokens: row.get(3),
                cost_usd: nano_to_usd(row.get::<_, i64>(4)),
            })
            .collect();

        Ok(TimeSeriesMetrics {
            organization_id: org_id,
            organization_name: org_name,
            period_start: start,
            period_end: end,
            granularity: granularity.to_string(),
            data,
        })
    }
}
