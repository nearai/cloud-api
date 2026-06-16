//! Analytics repository implementation for enterprise dashboard queries.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency.

use crate::pool::DbPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::admin::{
    AnalyticsRepository, ApiKeyMetrics, BillingSourceBreakdown, BillingSummary, MetricsSummary,
    ModelConsumptionPoint, ModelConsumptionTimeseries, ModelConsumptionTimeseriesQuery,
    ModelMetrics, ModelRevenueEntry, ModelRevenueQuery, ModelRevenueReport, OrgRevenueEntry,
    OrgRevenueQuery, OrgRevenueReport, OrganizationMetrics, PerformancePoint,
    PerformanceTimeseries, PerformanceTimeseriesQuery, PlatformMetrics, PlatformTimeSeriesMetrics,
    PlatformTimeSeriesPoint, RevenueSort, TimeSeriesMetrics, TimeSeriesPoint, TopModelMetrics,
    TopOrganizationMetrics, WorkspaceMetrics,
};
use services::common::RepositoryError;
use std::collections::BTreeMap;
use tokio_postgres::Error as PostgresError;
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

fn log_billing_summary_db_error(stage: &'static str, err: PostgresError) -> RepositoryError {
    if let Some(db_error) = err.as_db_error() {
        tracing::error!(
            billing_summary_stage = stage,
            sqlstate = db_error.code().code(),
            db_message = db_error.message(),
            db_table = db_error.table().unwrap_or(""),
            db_column = db_error.column().unwrap_or(""),
            "Billing summary database query failed"
        );
    } else {
        tracing::error!(
            billing_summary_stage = stage,
            sqlstate = "",
            db_message = "non-Postgres database error",
            db_table = "",
            db_column = "",
            "Billing summary database query failed"
        );
    }

    RepositoryError::DatabaseError(err.into())
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
                    COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
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
            total_cache_read_tokens: summary_row.get::<_, i64>(3),
            total_cost_usd: nano_to_usd(summary_row.get::<_, i64>(4)),
            unique_api_keys: summary_row.get::<_, i64>(5),
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
                    COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
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
                cache_read_tokens: row.get(5),
                cost_usd: nano_to_usd(row.get::<_, i64>(6)),
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
                    COALESCE(SUM(ul.input_tokens), 0)::bigint as input_tokens,
                    COALESCE(SUM(ul.output_tokens), 0)::bigint as output_tokens,
                    COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
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
                input_tokens: row.get(2),
                output_tokens: row.get(3),
                cache_read_tokens: row.get(4),
                cost_usd: nano_to_usd(row.get::<_, i64>(5)),
                avg_ttft_ms: row.get::<_, Option<f64>>(6),
                p95_ttft_ms: row.get::<_, Option<f64>>(7),
                avg_itl_ms: row.get::<_, Option<f64>>(8),
                p95_itl_ms: row.get::<_, Option<f64>>(9),
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

        // Counts: total active users/orgs (snapshot) + new signups (within the period) +
        // paying-org count (orgs with an active payment-type credit).
        let counts_row = client
            .query_one(
                r#"
                SELECT
                    (SELECT COUNT(*) FROM users WHERE is_active = true)::bigint as total_users,
                    (SELECT COUNT(*) FROM organizations WHERE is_active = true)::bigint as total_organizations,
                    (SELECT COUNT(*) FROM users
                        WHERE created_at >= $1 AND created_at < $2)::bigint as new_users,
                    (SELECT COUNT(*) FROM organizations
                        WHERE created_at >= $1 AND created_at < $2)::bigint as new_organizations,
                    (SELECT COUNT(DISTINCT olh.organization_id)
                        FROM organization_limits_history olh
                        JOIN organizations o ON o.id = olh.organization_id AND o.is_active = true
                        WHERE olh.credit_type = 'payment' AND olh.effective_until IS NULL)::bigint as paying_organizations
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let total_users: i64 = counts_row.get(0);
        let total_organizations: i64 = counts_row.get(1);
        let new_users: i64 = counts_row.get(2);
        let new_organizations: i64 = counts_row.get(3);
        let paying_organizations: i64 = counts_row.get(4);

        // Single-scan usage summary over the period: totals, the paid-vs-granted split
        // (attributed by org class), the verifiable-vs-external split (join models), the
        // error rate, and p95 TTFT. Verifiable split joins models on verifiability.
        let summary_row = client
            .query_one(
                r#"
                SELECT
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as revenue_nano,
                    (COALESCE(SUM(ul.input_tokens), 0) + COALESCE(SUM(ul.output_tokens), 0))::bigint as total_tokens,
                    COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
                    COUNT(DISTINCT ul.organization_id)::bigint as active_organizations,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
                    COUNT(*) FILTER (WHERE COALESCE(m.verifiable, false))::bigint as verifiable_requests,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
                    COUNT(*) FILTER (WHERE NOT COALESCE(m.verifiable, false))::bigint as external_requests,
                    COUNT(*) FILTER (WHERE ul.stop_reason IN ('provider_error', 'timeout'))::bigint as error_count,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms
                FROM organization_usage_log ul
                LEFT JOIN models m ON m.id = ul.model_id
                WHERE ul.created_at >= $1 AND ul.created_at < $2
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let total_requests: i64 = summary_row.get(0);
        let total_consumed_usd = nano_to_usd(summary_row.get::<_, i64>(1));
        let total_tokens: i64 = summary_row.get(2);
        let total_cache_read_tokens: i64 = summary_row.get(3);
        let active_organizations: i64 = summary_row.get(4);
        let verifiable_consumed_usd = nano_to_usd(summary_row.get::<_, i64>(5));
        let verifiable_requests: i64 = summary_row.get(6);
        let non_verifiable_consumed_usd = nano_to_usd(summary_row.get::<_, i64>(7));
        let non_verifiable_requests: i64 = summary_row.get(8);
        let error_count: i64 = summary_row.get(9);
        let p95_ttft_ms: Option<f64> = summary_row.get(10);
        let provider_error_or_timeout_rate = if total_requests > 0 {
            error_count as f64 / total_requests as f64
        } else {
            0.0
        };

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
            generated_at: Utc::now(),
            total_users,
            total_organizations,
            total_requests,
            total_consumed_usd,
            total_tokens,
            total_cache_read_tokens,
            new_users,
            new_organizations,
            active_organizations,
            paying_organizations,
            verifiable_consumed_usd,
            verifiable_requests,
            non_verifiable_consumed_usd,
            non_verifiable_requests,
            provider_error_or_timeout_rate,
            p95_ttft_ms,
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
                DATE_TRUNC('{date_trunc}', created_at)::text as date,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM organization_usage_log
            WHERE organization_id = $1
              AND created_at >= $2
              AND created_at < $3
            GROUP BY DATE_TRUNC('{date_trunc}', created_at)
            ORDER BY date ASC
            "#
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
                cache_read_tokens: row.get(4),
                cost_usd: nano_to_usd(row.get::<_, i64>(5)),
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

    async fn get_platform_timeseries(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        granularity: &str,
    ) -> Result<PlatformTimeSeriesMetrics, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        let date_trunc = match granularity {
            "hour" => "hour",
            "week" => "week",
            "month" => "month",
            _ => "day",
        };

        // Usage-derived buckets: requests, tokens, cost + verifiable/external split +
        // active orgs. One scan over usage_log joined to models.
        let usage_query = format!(
            r#"
            SELECT
                DATE_TRUNC('{date_trunc}', ul.created_at)::text as bucket,
                COUNT(*)::bigint as requests,
                (COALESCE(SUM(ul.input_tokens), 0) + COALESCE(SUM(ul.output_tokens), 0))::bigint as tokens,
                COALESCE(SUM(ul.total_cost), 0)::bigint as cost_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
                COUNT(DISTINCT ul.organization_id)::bigint as active_orgs
            FROM organization_usage_log ul
            LEFT JOIN models m ON m.id = ul.model_id
            WHERE ul.created_at >= $1 AND ul.created_at < $2
            GROUP BY DATE_TRUNC('{date_trunc}', ul.created_at)
            ORDER BY bucket ASC
            "#
        );

        let new_orgs_query = format!(
            r#"
            SELECT DATE_TRUNC('{date_trunc}', created_at)::text as bucket, COUNT(*)::bigint
            FROM organizations
            WHERE created_at >= $1 AND created_at < $2
            GROUP BY DATE_TRUNC('{date_trunc}', created_at)
            "#
        );

        let new_users_query = format!(
            r#"
            SELECT DATE_TRUNC('{date_trunc}', created_at)::text as bucket, COUNT(*)::bigint
            FROM users
            WHERE created_at >= $1 AND created_at < $2
            GROUP BY DATE_TRUNC('{date_trunc}', created_at)
            "#
        );

        let usage_rows = client
            .query(&usage_query, &[&start, &end])
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;
        let new_orgs_rows = client
            .query(&new_orgs_query, &[&start, &end])
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;
        let new_users_rows = client
            .query(&new_users_query, &[&start, &end])
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        // Merge the three result sets by bucket key. BTreeMap keeps ISO date keys sorted.
        let mut points: BTreeMap<String, PlatformTimeSeriesPoint> = BTreeMap::new();
        for row in &usage_rows {
            let date: String = row.get(0);
            points.insert(
                date.clone(),
                PlatformTimeSeriesPoint {
                    date,
                    requests: row.get(1),
                    tokens: row.get(2),
                    cost_usd: nano_to_usd(row.get::<_, i64>(3)),
                    verifiable_cost_usd: nano_to_usd(row.get::<_, i64>(4)),
                    non_verifiable_cost_usd: nano_to_usd(row.get::<_, i64>(5)),
                    active_organizations: row.get(6),
                    new_organizations: 0,
                    new_users: 0,
                },
            );
        }
        let empty_point = |date: String| PlatformTimeSeriesPoint {
            date,
            requests: 0,
            tokens: 0,
            cost_usd: 0.0,
            verifiable_cost_usd: 0.0,
            non_verifiable_cost_usd: 0.0,
            active_organizations: 0,
            new_organizations: 0,
            new_users: 0,
        };
        for row in &new_orgs_rows {
            let date: String = row.get(0);
            points
                .entry(date.clone())
                .or_insert_with(|| empty_point(date))
                .new_organizations = row.get(1);
        }
        for row in &new_users_rows {
            let date: String = row.get(0);
            points
                .entry(date.clone())
                .or_insert_with(|| empty_point(date))
                .new_users = row.get(1);
        }

        Ok(PlatformTimeSeriesMetrics {
            period_start: start,
            period_end: end,
            granularity: granularity.to_string(),
            data: points.into_values().collect(),
        })
    }

    async fn get_billing_summary(&self) -> Result<BillingSummary, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // Active credit LIMITS (caps) by type + paying/granted org counts. These are
        // ceilings from organization_limits_history, NOT payments/cash received. Joined to
        // organizations with is_active = true so soft-deleted orgs aren't counted.
        let limits_row = client
            .query_one(
                r#"
                SELECT
                    COALESCE(SUM(olh.spend_limit) FILTER (WHERE olh.credit_type = 'payment'), 0)::bigint as paid_limit,
                    COALESCE(SUM(olh.spend_limit) FILTER (WHERE olh.credit_type = 'grant'), 0)::bigint as grant_limit,
                    COUNT(DISTINCT olh.organization_id) FILTER (WHERE olh.credit_type = 'payment')::bigint as paying_orgs,
                    COUNT(DISTINCT olh.organization_id) FILTER (WHERE olh.credit_type = 'grant')::bigint as granted_orgs
                FROM organization_limits_history olh
                JOIN organizations o ON o.id = olh.organization_id AND o.is_active = true
                WHERE olh.effective_until IS NULL
                "#,
                &[],
            )
            .await
            .map_err(|e| log_billing_summary_db_error("active_limits", e))?;

        let active_paid_credit_limit_usd = nano_to_usd(limits_row.get::<_, i64>(0));
        let active_grant_credit_limit_usd = nano_to_usd(limits_row.get::<_, i64>(1));
        let paying_org_count: i64 = limits_row.get(2);
        let granted_org_count: i64 = limits_row.get(3);

        // All-time consumed cost. `total` (from the cached balance) is ALL usage
        // (inference + services); the inference/service splits come from their logs
        // and reconcile to the total.
        let consumed_row = client
            .query_one(
                r#"
                SELECT
                    (SELECT COALESCE(SUM(total_spent), 0) FROM organization_balance)::bigint as total_nano,
                    (SELECT COALESCE(SUM(total_cost), 0) FROM organization_usage_log)::bigint as inference_nano,
                    (SELECT COALESCE(SUM(total_cost), 0) FROM organization_service_usage_log)::bigint as service_nano
                "#,
                &[],
            )
            .await
            .map_err(|e| log_billing_summary_db_error("consumed_totals", e))?;
        let total_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(0));
        let inference_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(1));
        let service_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(2));

        // Active paid credit limit broken down by funding source (active orgs only).
        let source_rows = client
            .query(
                r#"
                SELECT
                    COALESCE(olh.source, 'unknown') as source,
                    COALESCE(SUM(olh.spend_limit) FILTER (WHERE olh.credit_type = 'payment'), 0)::bigint as paid_limit,
                    COUNT(DISTINCT olh.organization_id)::bigint as org_count
                FROM organization_limits_history olh
                JOIN organizations o ON o.id = olh.organization_id AND o.is_active = true
                WHERE olh.effective_until IS NULL
                GROUP BY olh.source
                ORDER BY paid_limit DESC
                "#,
                &[],
            )
            .await
            .map_err(|e| log_billing_summary_db_error("source_breakdown", e))?;

        let by_source: Vec<BillingSourceBreakdown> = source_rows
            .iter()
            .map(|row| BillingSourceBreakdown {
                source: row.get(0),
                paid_credit_limit_usd: nano_to_usd(row.get::<_, i64>(1)),
                org_count: row.get(2),
            })
            .collect();

        Ok(BillingSummary {
            generated_at: Utc::now(),
            active_paid_credit_limit_usd,
            active_grant_credit_limit_usd,
            total_consumed_usd,
            inference_consumed_usd,
            service_consumed_usd,
            paying_org_count,
            granted_org_count,
            by_source,
        })
    }

    async fn get_model_revenue(
        &self,
        query: ModelRevenueQuery,
    ) -> Result<ModelRevenueReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // Sort column from a fixed allowlist (never interpolate user input).
        let sort_col = match query.sort {
            RevenueSort::Revenue => "revenue_nano",
            RevenueSort::Requests => "requests",
            RevenueSort::Tokens => "tokens",
        };
        // Shared WHERE; optional filters via `$n::type IS NULL OR …`. `model_search`
        // is a case-insensitive substring (the `%…%` wrapping is the bind value).
        let where_clause = r#"
            WHERE ul.created_at >= $1 AND ul.created_at < $2
              AND ($3::bool IS NULL OR COALESCE(m.verifiable, false) = $3)
              AND ($4::text IS NULL OR m.provider_type = $4)
              AND ($5::text IS NULL OR ul.model_name ILIKE $5)
        "#;
        let model_like = query.model_search.as_ref().map(|s| format!("%{s}%"));

        // Total = number of matching model groups (correct even when offset >= total).
        let count_sql = format!(
            "SELECT COUNT(*)::bigint FROM (SELECT 1 FROM organization_usage_log ul \
             LEFT JOIN models m ON m.id = ul.model_id {where_clause} GROUP BY ul.model_name) t"
        );
        let total: i64 = client
            .query_one(
                &count_sql,
                &[
                    &query.start,
                    &query.end,
                    &query.verifiable,
                    &query.provider_type,
                    &model_like,
                ],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?
            .get(0);

        let data_sql = format!(
            r#"
            SELECT
                ul.model_name,
                COALESCE(SUM(ul.total_cost), 0)::bigint as revenue_nano,
                COUNT(*)::bigint as requests,
                (COALESCE(SUM(ul.input_tokens), 0) + COALESCE(SUM(ul.output_tokens), 0))::bigint as tokens,
                COUNT(DISTINCT ul.organization_id)::bigint as unique_orgs,
                BOOL_OR(COALESCE(m.verifiable, false)) as verifiable,
                MAX(m.provider_type) as provider_type,
                AVG(ul.ttft_ms)::double precision as avg_ttft_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms
            FROM organization_usage_log ul
            LEFT JOIN models m ON m.id = ul.model_id
            {where_clause}
            GROUP BY ul.model_name
            ORDER BY {sort_col} DESC
            LIMIT $6 OFFSET $7
            "#
        );
        let rows = client
            .query(
                &data_sql,
                &[
                    &query.start,
                    &query.end,
                    &query.verifiable,
                    &query.provider_type,
                    &model_like,
                    &query.limit,
                    &query.offset,
                ],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let data: Vec<ModelRevenueEntry> = rows
            .iter()
            .map(|row| ModelRevenueEntry {
                model_name: row.get(0),
                consumed_cost_usd: nano_to_usd(row.get::<_, i64>(1)),
                requests: row.get(2),
                tokens: row.get(3),
                unique_orgs: row.get(4),
                verifiable: row.get::<_, Option<bool>>(5).unwrap_or(false),
                provider_type: row.get(6),
                avg_ttft_ms: row.get(7),
                p95_ttft_ms: row.get(8),
            })
            .collect();

        Ok(ModelRevenueReport {
            period_start: query.start,
            period_end: query.end,
            data,
            total,
            limit: query.limit,
            offset: query.offset,
        })
    }

    async fn get_org_revenue(
        &self,
        query: OrgRevenueQuery,
    ) -> Result<OrgRevenueReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        let sort_col = match query.sort {
            RevenueSort::Revenue => "revenue_nano",
            RevenueSort::Requests => "requests",
            RevenueSort::Tokens => "tokens",
        };
        // `is_paying` is a current-state flag (org has an active payment credit), used
        // both as an output column and as the optional `paying` filter (via HAVING).
        // `search` is a case-insensitive substring on org name (the `%…%` is the bind).
        let org_like = query.search.as_ref().map(|s| format!("%{s}%"));
        let cte_and_from = r#"
            WITH paying AS (
                SELECT DISTINCT organization_id
                FROM organization_limits_history
                WHERE credit_type = 'payment' AND effective_until IS NULL
            )
            SELECT
                o.id as organization_id,
                o.name as organization_name,
                COALESCE(SUM(ul.total_cost), 0)::bigint as revenue_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
                COUNT(ul.id)::bigint as requests,
                (COALESCE(SUM(ul.input_tokens), 0) + COALESCE(SUM(ul.output_tokens), 0))::bigint as tokens,
                COUNT(DISTINCT ul.model_name)::bigint as models_used,
                BOOL_OR(p.organization_id IS NOT NULL) as is_paying,
                MAX(ul.created_at) as last_usage_at
            FROM organizations o
            INNER JOIN organization_usage_log ul ON ul.organization_id = o.id
                AND ul.created_at >= $1 AND ul.created_at < $2
            LEFT JOIN models m ON m.id = ul.model_id
            LEFT JOIN paying p ON p.organization_id = o.id
            WHERE ($4::text IS NULL OR o.name ILIKE $4)
            GROUP BY o.id, o.name
            HAVING ($3::bool IS NULL OR BOOL_OR(p.organization_id IS NOT NULL) = $3)
        "#;

        // Total = matching org groups after HAVING (correct when offset >= total).
        let count_sql = format!("SELECT COUNT(*)::bigint FROM ({cte_and_from}) t");
        let total: i64 = client
            .query_one(
                &count_sql,
                &[&query.start, &query.end, &query.paying, &org_like],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?
            .get(0);

        let data_sql = format!("{cte_and_from} ORDER BY {sort_col} DESC LIMIT $5 OFFSET $6");
        let rows = client
            .query(
                &data_sql,
                &[
                    &query.start,
                    &query.end,
                    &query.paying,
                    &org_like,
                    &query.limit,
                    &query.offset,
                ],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let data: Vec<OrgRevenueEntry> = rows
            .iter()
            .map(|row| OrgRevenueEntry {
                organization_id: row.get(0),
                organization_name: row.get(1),
                consumed_cost_usd: nano_to_usd(row.get::<_, i64>(2)),
                verifiable_consumed_usd: nano_to_usd(row.get::<_, i64>(3)),
                non_verifiable_consumed_usd: nano_to_usd(row.get::<_, i64>(4)),
                requests: row.get(5),
                tokens: row.get(6),
                models_used: row.get(7),
                is_paying: row.get::<_, Option<bool>>(8).unwrap_or(false),
                last_usage_at: row.get(9),
            })
            .collect();

        Ok(OrgRevenueReport {
            period_start: query.start,
            period_end: query.end,
            data,
            total,
            limit: query.limit,
            offset: query.offset,
        })
    }

    async fn get_model_consumption_timeseries(
        &self,
        query: ModelConsumptionTimeseriesQuery,
    ) -> Result<ModelConsumptionTimeseries, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // granularity is already an allowlisted &'static str from the handler
        let date_trunc = query.granularity.as_str();

        // Step 1: identify the top-N model_ids by total cost in the period.
        // We use model_id (UUID) as the grouping key to survive model renames.
        let top_ids_rows = client
            .query(
                r#"
                SELECT model_id
                FROM organization_usage_log
                WHERE created_at >= $1 AND created_at < $2
                GROUP BY model_id
                ORDER BY SUM(total_cost) DESC
                LIMIT $3
                "#,
                &[&query.start, &query.end, &query.top_n],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let top_ids: Vec<uuid::Uuid> = top_ids_rows.iter().map(|r| r.get(0)).collect();

        // Step 2: time-bucketed aggregation. Models in top_ids get their current
        // canonical name from models.model_name; all others collapse to "Other".
        let bucket_query = format!(
            r#"
            SELECT
                DATE_TRUNC('{date_trunc}', ul.created_at)::text AS bucket,
                CASE
                    WHEN ul.model_id = ANY($3) THEN COALESCE(m.model_name, ul.model_name)
                    ELSE 'Other'
                END AS model_label,
                COALESCE(SUM(ul.total_cost), 0)::bigint AS cost_nano,
                COUNT(*)::bigint AS requests,
                COALESCE(SUM(ul.total_tokens), 0)::bigint AS tokens
            FROM organization_usage_log ul
            LEFT JOIN models m ON m.id = ul.model_id
            WHERE ul.created_at >= $1 AND ul.created_at < $2
            GROUP BY 1, 2
            ORDER BY 1 ASC, cost_nano DESC
            "#
        );

        let rows = client
            .query(&bucket_query, &[&query.start, &query.end, &top_ids])
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        // Accumulate total cost per label across all buckets, then sort descending
        // so model_labels reflects true global top-N rank (not first-bucket order).
        let mut label_totals: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        let data: Vec<ModelConsumptionPoint> = rows
            .iter()
            .map(|row| {
                let label: String = row.get(1);
                let cost_nano: i64 = row.get(2);
                *label_totals.entry(label.clone()).or_insert(0) += cost_nano;
                ModelConsumptionPoint {
                    bucket: row.get(0),
                    model_label: label,
                    consumed_cost_usd: nano_to_usd(cost_nano),
                    requests: row.get(3),
                    tokens: row.get(4),
                }
            })
            .collect();

        // Sort: top models by total period cost DESC; "Other" always last.
        let mut model_labels_ordered: Vec<String> = label_totals.keys().cloned().collect();
        model_labels_ordered.sort_by(|a, b| {
            if a == "Other" {
                return std::cmp::Ordering::Greater;
            }
            if b == "Other" {
                return std::cmp::Ordering::Less;
            }
            let ta = label_totals.get(a).copied().unwrap_or(0);
            let tb = label_totals.get(b).copied().unwrap_or(0);
            tb.cmp(&ta)
        });

        Ok(ModelConsumptionTimeseries {
            period_start: query.start,
            period_end: query.end,
            granularity: query.granularity,
            model_labels: model_labels_ordered,
            data,
        })
    }

    async fn get_performance_timeseries(
        &self,
        query: PerformanceTimeseriesQuery,
    ) -> Result<PerformanceTimeseries, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        let date_trunc = query.granularity.as_str();

        // Optional model_name filter: $3::text IS NULL OR ul.model_name = $3
        let sql = format!(
            r#"
            SELECT
                DATE_TRUNC('{date_trunc}', ul.created_at)::text AS bucket,
                COUNT(*)::bigint AS requests,
                COALESCE(SUM(ul.total_tokens), 0)::bigint AS total_tokens,
                COALESCE(SUM(ul.output_tokens), 0)::bigint AS output_tokens,
                COUNT(*) FILTER (WHERE ul.ttft_ms IS NOT NULL)::bigint AS ttft_sample_count,
                PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision AS p50_ttft_ms,
                PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision AS p95_ttft_ms,
                PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision AS p99_ttft_ms,
                CASE
                    WHEN COUNT(*) FILTER (WHERE ul.stop_reason IS NOT NULL) = 0 THEN NULL
                    ELSE COUNT(*) FILTER (WHERE ul.stop_reason IN ('provider_error', 'timeout'))::float8
                         / COUNT(*) FILTER (WHERE ul.stop_reason IS NOT NULL)::float8
                END AS error_rate
            FROM organization_usage_log ul
            WHERE ul.created_at >= $1 AND ul.created_at < $2
              AND ($3::text IS NULL OR ul.model_name = $3)
            GROUP BY 1
            ORDER BY 1 ASC
            "#
        );

        let rows = client
            .query(&sql, &[&query.start, &query.end, &query.model_name])
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let data: Vec<PerformancePoint> = rows
            .iter()
            .map(|row| PerformancePoint {
                bucket: row.get(0),
                requests: row.get(1),
                total_tokens: row.get(2),
                output_tokens: row.get(3),
                ttft_sample_count: row.get(4),
                p50_ttft_ms: row.get(5),
                p95_ttft_ms: row.get(6),
                p99_ttft_ms: row.get(7),
                error_rate: row.get(8),
            })
            .collect();

        Ok(PerformanceTimeseries {
            period_start: query.start,
            period_end: query.end,
            granularity: query.granularity,
            model_filter: query.model_name,
            data,
        })
    }
}
