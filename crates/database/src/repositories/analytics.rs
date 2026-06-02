//! Analytics repository implementation for enterprise dashboard queries.
//!
//! All costs use fixed scale 9 (nano-dollars) and USD currency.

use crate::pool::DbPool;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use services::admin::{
    AnalyticsRepository, ApiKeyMetrics, BillingSourceBreakdown, BillingSummary, MetricsSummary,
    ModelMetrics, ModelRevenueEntry, ModelRevenueReport, OrgRevenueEntry, OrgRevenueReport,
    OrganizationMetrics, PlatformMetrics, PlatformTimeSeriesMetrics, PlatformTimeSeriesPoint,
    TimeSeriesMetrics, TimeSeriesPoint, TopModelMetrics, TopOrganizationMetrics, WorkspaceMetrics,
};
use services::common::RepositoryError;
use std::collections::BTreeMap;
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
                    (SELECT COUNT(DISTINCT organization_id) FROM organization_limits_history
                        WHERE credit_type = 'payment' AND effective_until IS NULL)::bigint as paying_organizations
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
        // error rate, and p95 TTFT. `paying` = orgs with an active payment-type credit.
        let summary_row = client
            .query_one(
                r#"
                WITH paying AS (
                    SELECT DISTINCT organization_id
                    FROM organization_limits_history
                    WHERE credit_type = 'payment' AND effective_until IS NULL
                )
                SELECT
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as revenue_nano,
                    COALESCE(SUM(ul.input_tokens + ul.output_tokens), 0)::bigint as total_tokens,
                    COALESCE(SUM(ul.cache_read_tokens), 0)::bigint as cache_read_tokens,
                    COUNT(DISTINCT ul.organization_id)::bigint as active_organizations,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NOT NULL), 0)::bigint as paid_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NULL), 0)::bigint as granted_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
                    COUNT(*) FILTER (WHERE COALESCE(m.verifiable, false))::bigint as verifiable_requests,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
                    COUNT(*) FILTER (WHERE NOT COALESCE(m.verifiable, false))::bigint as external_requests,
                    COUNT(*) FILTER (WHERE ul.stop_reason IN ('provider_error', 'timeout'))::bigint as error_count,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms
                FROM organization_usage_log ul
                LEFT JOIN models m ON m.id = ul.model_id
                LEFT JOIN paying p ON p.organization_id = ul.organization_id
                WHERE ul.created_at >= $1 AND ul.created_at < $2
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let total_requests: i64 = summary_row.get(0);
        let total_revenue_usd = nano_to_usd(summary_row.get::<_, i64>(1));
        let total_tokens: i64 = summary_row.get(2);
        let total_cache_read_tokens: i64 = summary_row.get(3);
        let active_organizations: i64 = summary_row.get(4);
        let paid_revenue_usd = nano_to_usd(summary_row.get::<_, i64>(5));
        let granted_revenue_usd = nano_to_usd(summary_row.get::<_, i64>(6));
        let verifiable_revenue_usd = nano_to_usd(summary_row.get::<_, i64>(7));
        let verifiable_requests: i64 = summary_row.get(8);
        let external_revenue_usd = nano_to_usd(summary_row.get::<_, i64>(9));
        let external_requests: i64 = summary_row.get(10);
        let error_count: i64 = summary_row.get(11);
        let p95_ttft_ms: Option<f64> = summary_row.get(12);
        let error_rate = if total_requests > 0 {
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
            total_users,
            total_organizations,
            total_requests,
            total_revenue_usd,
            total_tokens,
            total_cache_read_tokens,
            new_users,
            new_organizations,
            active_organizations,
            paying_organizations,
            paid_revenue_usd,
            granted_revenue_usd,
            verifiable_revenue_usd,
            verifiable_requests,
            external_revenue_usd,
            external_requests,
            error_rate,
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

        // Usage-derived buckets: requests, tokens, cost + paid/granted + verifiable/external
        // splits + active orgs. One scan over usage_log joined to models and the paying-org set.
        let usage_query = format!(
            r#"
            WITH paying AS (
                SELECT DISTINCT organization_id
                FROM organization_limits_history
                WHERE credit_type = 'payment' AND effective_until IS NULL
            )
            SELECT
                DATE_TRUNC('{date_trunc}', ul.created_at)::text as bucket,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(ul.input_tokens + ul.output_tokens), 0)::bigint as tokens,
                COALESCE(SUM(ul.total_cost), 0)::bigint as cost_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NOT NULL), 0)::bigint as paid_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NULL), 0)::bigint as granted_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
                COALESCE(SUM(ul.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
                COUNT(DISTINCT ul.organization_id)::bigint as active_orgs
            FROM organization_usage_log ul
            LEFT JOIN models m ON m.id = ul.model_id
            LEFT JOIN paying p ON p.organization_id = ul.organization_id
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
                    paid_cost_usd: nano_to_usd(row.get::<_, i64>(4)),
                    granted_cost_usd: nano_to_usd(row.get::<_, i64>(5)),
                    verifiable_cost_usd: nano_to_usd(row.get::<_, i64>(6)),
                    external_cost_usd: nano_to_usd(row.get::<_, i64>(7)),
                    active_organizations: row.get(8),
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
            paid_cost_usd: 0.0,
            granted_cost_usd: 0.0,
            verifiable_cost_usd: 0.0,
            external_cost_usd: 0.0,
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

    async fn get_billing_summary(
        &self,
        as_of: DateTime<Utc>,
    ) -> Result<BillingSummary, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        // Provisioned credits (active caps) + paying/granted org counts.
        let prov_row = client
            .query_one(
                r#"
                SELECT
                    COALESCE(SUM(spend_limit) FILTER (WHERE credit_type = 'payment'), 0)::bigint as paid_provisioned,
                    COALESCE(SUM(spend_limit) FILTER (WHERE credit_type = 'grant'), 0)::bigint as granted_provisioned,
                    COUNT(DISTINCT organization_id) FILTER (WHERE credit_type = 'payment')::bigint as paying_orgs,
                    COUNT(DISTINCT organization_id) FILTER (WHERE credit_type = 'grant')::bigint as granted_orgs
                FROM organization_limits_history
                WHERE effective_until IS NULL
                "#,
                &[],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let paid_provisioned_usd = nano_to_usd(prov_row.get::<_, i64>(0));
        let granted_provisioned_usd = nano_to_usd(prov_row.get::<_, i64>(1));
        let paying_org_count: i64 = prov_row.get(2);
        let granted_org_count: i64 = prov_row.get(3);

        // Consumption (all-time, from the cached balance table): total + paying-org subset.
        let consumed_row = client
            .query_one(
                r#"
                WITH paying AS (
                    SELECT DISTINCT organization_id
                    FROM organization_limits_history
                    WHERE credit_type = 'payment' AND effective_until IS NULL
                )
                SELECT
                    COALESCE(SUM(ob.total_spent), 0)::bigint as total_consumed,
                    COALESCE(SUM(ob.total_spent) FILTER (WHERE p.organization_id IS NOT NULL), 0)::bigint as paid_consumed
                FROM organization_balance ob
                LEFT JOIN paying p ON p.organization_id = ob.organization_id
                "#,
                &[],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let total_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(0));
        let paid_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(1));
        let unspent_paid_balance_usd = (paid_provisioned_usd - paid_consumed_usd).max(0.0);

        // Annualized run-rate from the last 30 days of paid consumption.
        let window_start = as_of - chrono::Duration::days(30);
        let run_rate_row = client
            .query_one(
                r#"
                WITH paying AS (
                    SELECT DISTINCT organization_id
                    FROM organization_limits_history
                    WHERE credit_type = 'payment' AND effective_until IS NULL
                )
                SELECT COALESCE(SUM(ul.total_cost), 0)::bigint
                FROM organization_usage_log ul
                JOIN paying p ON p.organization_id = ul.organization_id
                WHERE ul.created_at >= $1 AND ul.created_at < $2
                "#,
                &[&window_start, &as_of],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;
        let run_rate_usd = nano_to_usd(run_rate_row.get::<_, i64>(0)) * 12.17;

        // Provisioned paid credits broken down by funding source.
        let source_rows = client
            .query(
                r#"
                SELECT
                    COALESCE(source, 'unknown') as source,
                    COALESCE(SUM(spend_limit) FILTER (WHERE credit_type = 'payment'), 0)::bigint as paid_provisioned,
                    COUNT(DISTINCT organization_id)::bigint as org_count
                FROM organization_limits_history
                WHERE effective_until IS NULL
                GROUP BY source
                ORDER BY paid_provisioned DESC
                "#,
                &[],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let by_source: Vec<BillingSourceBreakdown> = source_rows
            .iter()
            .map(|row| BillingSourceBreakdown {
                source: row.get(0),
                paid_provisioned_usd: nano_to_usd(row.get::<_, i64>(1)),
                org_count: row.get(2),
            })
            .collect();

        Ok(BillingSummary {
            paid_provisioned_usd,
            granted_provisioned_usd,
            total_consumed_usd,
            paid_consumed_usd,
            unspent_paid_balance_usd,
            paying_org_count,
            granted_org_count,
            run_rate_usd,
            by_source,
        })
    }

    async fn get_model_revenue(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<ModelRevenueReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        let rows = client
            .query(
                r#"
                WITH paying AS (
                    SELECT DISTINCT organization_id
                    FROM organization_limits_history
                    WHERE credit_type = 'payment' AND effective_until IS NULL
                )
                SELECT
                    ul.model_name,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as revenue_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NOT NULL), 0)::bigint as paid_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NULL), 0)::bigint as granted_nano,
                    COUNT(*)::bigint as requests,
                    COALESCE(SUM(ul.input_tokens + ul.output_tokens), 0)::bigint as tokens,
                    COUNT(DISTINCT ul.organization_id)::bigint as unique_orgs,
                    BOOL_OR(COALESCE(m.verifiable, false)) as verifiable,
                    MAX(m.provider_type) as provider_type,
                    AVG(ul.ttft_ms)::double precision as avg_ttft_ms,
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms
                FROM organization_usage_log ul
                LEFT JOIN models m ON m.id = ul.model_id
                LEFT JOIN paying p ON p.organization_id = ul.organization_id
                WHERE ul.created_at >= $1 AND ul.created_at < $2
                GROUP BY ul.model_name
                ORDER BY revenue_nano DESC
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let models: Vec<ModelRevenueEntry> = rows
            .iter()
            .map(|row| ModelRevenueEntry {
                model_name: row.get(0),
                revenue_usd: nano_to_usd(row.get::<_, i64>(1)),
                paid_revenue_usd: nano_to_usd(row.get::<_, i64>(2)),
                granted_revenue_usd: nano_to_usd(row.get::<_, i64>(3)),
                requests: row.get(4),
                tokens: row.get(5),
                unique_orgs: row.get(6),
                verifiable: row.get::<_, Option<bool>>(7).unwrap_or(false),
                provider_type: row.get(8),
                avg_ttft_ms: row.get(9),
                p95_ttft_ms: row.get(10),
            })
            .collect();

        Ok(ModelRevenueReport {
            period_start: start,
            period_end: end,
            models,
        })
    }

    async fn get_org_revenue(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<OrgRevenueReport, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| RepositoryError::PoolError(e.into()))?;

        let rows = client
            .query(
                r#"
                WITH paying AS (
                    SELECT DISTINCT organization_id
                    FROM organization_limits_history
                    WHERE credit_type = 'payment' AND effective_until IS NULL
                )
                SELECT
                    o.id as organization_id,
                    o.name as organization_name,
                    COALESCE(SUM(ul.total_cost), 0)::bigint as revenue_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NOT NULL), 0)::bigint as paid_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE p.organization_id IS NULL), 0)::bigint as granted_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
                    COALESCE(SUM(ul.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
                    COUNT(ul.id)::bigint as requests,
                    COALESCE(SUM(ul.input_tokens + ul.output_tokens), 0)::bigint as tokens,
                    COUNT(DISTINCT ul.model_name)::bigint as models_used,
                    BOOL_OR(p.organization_id IS NOT NULL) as is_paying,
                    MAX(ul.created_at) as last_usage_at
                FROM organizations o
                INNER JOIN organization_usage_log ul ON ul.organization_id = o.id
                    AND ul.created_at >= $1 AND ul.created_at < $2
                LEFT JOIN models m ON m.id = ul.model_id
                LEFT JOIN paying p ON p.organization_id = o.id
                GROUP BY o.id, o.name
                ORDER BY revenue_nano DESC
                "#,
                &[&start, &end],
            )
            .await
            .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

        let organizations: Vec<OrgRevenueEntry> = rows
            .iter()
            .map(|row| OrgRevenueEntry {
                organization_id: row.get(0),
                organization_name: row.get(1),
                revenue_usd: nano_to_usd(row.get::<_, i64>(2)),
                paid_revenue_usd: nano_to_usd(row.get::<_, i64>(3)),
                granted_revenue_usd: nano_to_usd(row.get::<_, i64>(4)),
                verifiable_revenue_usd: nano_to_usd(row.get::<_, i64>(5)),
                external_revenue_usd: nano_to_usd(row.get::<_, i64>(6)),
                requests: row.get(7),
                tokens: row.get(8),
                models_used: row.get(9),
                is_paying: row.get::<_, Option<bool>>(10).unwrap_or(false),
                last_usage_at: row.get(11),
            })
            .collect();

        Ok(OrgRevenueReport {
            period_start: start,
            period_end: end,
            organizations,
        })
    }
}
