//! Platform-wide reports for admin dashboards.

use super::{nano_to_usd, provider_attribution};
use chrono::{DateTime, Utc};
use services::admin::{
    PlatformMetrics, PlatformTimeSeriesMetrics, PlatformTimeSeriesPoint, TopModelMetrics,
    TopOrganizationMetrics,
};
use services::common::RepositoryError;
use std::collections::BTreeMap;

pub(super) async fn get_platform_metrics(
    client: &tokio_postgres::Client,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<PlatformMetrics, RepositoryError> {
    // Counts: total active users/orgs (snapshot) + new signups (within the period) +
    // paying-org count (orgs with an active prepaid or contract credit).
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
                        WHERE (olh.credit_type = 'payment'
                            OR (olh.credit_type = 'postpay' AND olh.spend_limit > 0))
                            AND olh.effective_until IS NULL)::bigint as paying_organizations
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
                    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY ul.ttft_ms)::double precision as p95_ttft_ms,
                    COUNT(*) FILTER (WHERE ul.stop_reason = 'incomplete')::bigint as incomplete_count
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
    let incomplete_count: i64 = summary_row.get(11);
    let provider_error_or_timeout_rate = if total_requests > 0 {
        error_count as f64 / total_requests as f64
    } else {
        0.0
    };
    let incomplete_stream_rate = if total_requests > 0 {
        incomplete_count as f64 / total_requests as f64
    } else {
        0.0
    };

    let provider_usage =
        provider_attribution::get_platform_provider_usage(client, start, end).await?;

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
        incomplete_stream_rate,
        p95_ttft_ms,
        provider_usage,
        top_models,
        top_organizations,
    })
}

pub(super) async fn get_platform_timeseries(
    client: &tokio_postgres::Client,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    granularity: &str,
) -> Result<PlatformTimeSeriesMetrics, RepositoryError> {
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
