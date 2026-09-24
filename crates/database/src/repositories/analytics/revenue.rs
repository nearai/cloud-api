//! Revenue and billing reports for admin dashboards.

use super::{approx_percentile, arm, weighted_mean};
use super::{nano_to_usd, pagination, provider_attribution};
use crate::repositories::usage_hourly::with_usage_rows;
use crate::repositories::utils::map_db_error;
use chrono::Utc;
use services::admin::{
    BillingSourceBreakdown, BillingSummary, ModelRevenueEntry, ModelRevenueQuery,
    ModelRevenueReport, OrgRevenueEntry, OrgRevenueQuery, OrgRevenueReport, RevenueDensityModelRow,
    RevenueDensityQuery, RevenueDensityReport, RevenueSort,
};
use services::common::RepositoryError;
use std::time::Instant;
use tokio_postgres::Error as PostgresError;
use tokio_postgres::Transaction;

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

    map_db_error(err)
}

pub(super) async fn get_billing_summary(
    tx: &Transaction<'_>,
    deadline: Instant,
) -> Result<BillingSummary, RepositoryError> {
    // Active credit LIMITS (caps) by type + paying/granted org counts. Postpay
    // identifies a paying organization, but its safety ceiling is deliberately
    // excluded from active_paid_credit_limit_usd because it is not prepaid cash.
    // Joined to active organizations so soft-deleted orgs aren't counted.
    arm(tx, deadline).await?;
    let limits_row = tx
            .query_one(
                r#"
                SELECT
                    COALESCE(SUM(olh.spend_limit) FILTER (WHERE olh.credit_type = 'payment'), 0)::bigint as paid_limit,
                    COALESCE(SUM(olh.spend_limit) FILTER (WHERE olh.credit_type = 'grant'), 0)::bigint as grant_limit,
                    COUNT(DISTINCT olh.organization_id) FILTER (
                        WHERE olh.credit_type = 'payment'
                            OR (olh.credit_type = 'postpay' AND olh.spend_limit > 0)
                    )::bigint as paying_orgs,
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

    // All-time consumed cost. `total` comes from the live cached balance (all usage). The
    // inference split sums `usage_rows` over all time (exact, like a raw sum) but excludes raw
    // rows V0045 deduplicated that the balance still counts (spec §6.2); the service split
    // is live raw. The splits therefore need not add up to the total.
    arm(tx, deadline).await?;
    let consumed_row = tx
        .query_one(
            &with_usage_rows(
                "'-infinity'::timestamptz",
                "'infinity'::timestamptz",
                r#"
            SELECT
                (SELECT COALESCE(SUM(total_spent), 0) FROM organization_balance)::bigint as total_nano,
                (SELECT COALESCE(SUM(total_cost), 0) FROM usage_rows)::bigint as inference_nano,
                (SELECT COALESCE(SUM(total_cost), 0) FROM organization_service_usage_log)::bigint as service_nano
            "#,
            ),
            &[],
        )
        .await
        .map_err(|e| log_billing_summary_db_error("consumed_totals", e))?;
    let total_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(0));
    let inference_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(1));
    let service_consumed_usd = nano_to_usd(consumed_row.get::<_, i64>(2));

    // Active prepaid credit limit broken down by funding source (active orgs only).
    // Contract postpay ceilings are intentionally not part of this cash-like metric.
    arm(tx, deadline).await?;
    let source_rows = tx
        .query(
            r#"
                SELECT
                    COALESCE(olh.source, 'unknown') as source,
                    COALESCE(SUM(olh.spend_limit), 0)::bigint as paid_limit,
                    COUNT(DISTINCT olh.organization_id)::bigint as org_count
                FROM organization_limits_history olh
                JOIN organizations o ON o.id = olh.organization_id AND o.is_active = true
                WHERE olh.effective_until IS NULL AND olh.credit_type = 'payment'
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

pub(super) async fn get_model_revenue(
    tx: &Transaction<'_>,
    deadline: Instant,
    query: ModelRevenueQuery,
) -> Result<ModelRevenueReport, RepositoryError> {
    // Sort column from a fixed allowlist (never interpolate user input).
    let sort_col = match query.sort {
        RevenueSort::Revenue => "revenue_nano",
        RevenueSort::Requests => "requests",
        RevenueSort::Tokens => "tokens",
    };
    // Shared WHERE; optional filters via `$n::type IS NULL OR …`. `model_search`
    // is a case-insensitive substring (the `%…%` wrapping is the bind value).
    let where_clause = r#"
        WHERE ($3::bool IS NULL OR COALESCE(m.verifiable, false) = $3)
          AND ($4::text IS NULL OR COALESCE(uh.served_provider_type, m.provider_type) = $4)
          AND ($5::text IS NULL OR uh.model_name ILIKE $5)
    "#;
    let model_like = query.model_search.as_ref().map(|s| format!("%{s}%"));
    let avg_ttft = weighted_mean("uh.ttft_sum_ms", "uh.ttft_count");
    let p95_ttft = approx_percentile("uh.ttft_p95_ms", "uh.ttft_count");

    let data_sql = with_usage_rows(
        "$1",
        "$2",
        &format!(
            r#"
        SELECT
            uh.model_name,
            COALESCE(SUM(uh.total_cost), 0)::bigint as revenue_nano,
            COALESCE(SUM(uh.request_count), 0)::bigint as requests,
            (COALESCE(SUM(uh.input_tokens), 0) + COALESCE(SUM(uh.output_tokens), 0))::bigint as tokens,
            COUNT(DISTINCT uh.organization_id)::bigint as unique_orgs,
            BOOL_OR(COALESCE(m.verifiable, false)) as verifiable,
            MAX(m.provider_type) as provider_type,
            {avg_ttft} as avg_ttft_ms,
            {p95_ttft} as p95_ttft_ms,
            COALESCE(SUM(uh.request_count) FILTER (WHERE uh.served_via_fallback), 0)::bigint as fallback_requests,
            COALESCE(SUM(uh.total_cost) FILTER (WHERE uh.served_via_fallback), 0)::bigint as fallback_cost_nano,
            COUNT(*) OVER ()::bigint as total_groups
        FROM usage_rows uh
        LEFT JOIN models m ON m.id = uh.model_id
        {where_clause}
        GROUP BY uh.model_name
        ORDER BY {sort_col} DESC
        LIMIT $6 OFFSET $7
        "#
        ),
    );
    // One bind list: the count shares the data query's filters ($1-$5).
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 7] = [
        &query.start,
        &query.end,
        &query.verifiable,
        &query.provider_type,
        &model_like,
        &query.limit,
        &query.offset,
    ];
    arm(tx, deadline).await?;
    let rows = tx.query(&data_sql, &params).await.map_err(map_db_error)?;
    let count_sql = with_usage_rows(
        "$1",
        "$2",
        &format!(
            "SELECT COUNT(*)::bigint FROM (SELECT 1 FROM usage_rows uh \
             LEFT JOIN models m ON m.id = uh.model_id {where_clause} GROUP BY uh.model_name) t"
        ),
    );
    let total = pagination::page_total(
        tx,
        deadline,
        &rows,
        (query.limit, query.offset),
        &count_sql,
        &params[..5],
    )
    .await?;

    let mut data: Vec<ModelRevenueEntry> = rows
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
            served_provider_breakdown: Vec::new(),
            fallback_requests: row.get(9),
            fallback_consumed_cost_usd: nano_to_usd(row.get::<_, i64>(10)),
        })
        .collect();

    provider_attribution::load_model_provider_breakdowns(
        tx,
        deadline,
        &query,
        where_clause,
        &model_like,
        &mut data,
    )
    .await?;

    Ok(ModelRevenueReport {
        period_start: query.start,
        period_end: query.end,
        data,
        total,
        limit: query.limit,
        offset: query.offset,
    })
}

pub(super) async fn get_org_revenue(
    tx: &Transaction<'_>,
    deadline: Instant,
    query: OrgRevenueQuery,
) -> Result<OrgRevenueReport, RepositoryError> {
    let (start, end) = (query.start, query.end);
    let sort_col = match query.sort {
        RevenueSort::Revenue => "revenue_nano",
        RevenueSort::Requests => "requests",
        RevenueSort::Tokens => "tokens",
    };
    // `is_paying` is a current-state flag (org has an active prepaid or contract credit), used
    // both as an output column and as the optional `paying` filter (via HAVING); it stays a
    // live join (spec §6.2). `search` is a case-insensitive substring on org name (the `%…%`
    // is the bind).
    let org_like = query.search.as_ref().map(|s| format!("%{s}%"));
    let cte_and_from = r#"
        WITH paying AS (
            SELECT DISTINCT organization_id
            FROM organization_limits_history
            WHERE (credit_type = 'payment'
                OR (credit_type = 'postpay' AND spend_limit > 0))
                AND effective_until IS NULL
        )
        SELECT
            o.id as organization_id,
            o.name as organization_name,
            COALESCE(SUM(uh.total_cost), 0)::bigint as revenue_nano,
            COALESCE(SUM(uh.total_cost) FILTER (WHERE COALESCE(m.verifiable, false)), 0)::bigint as verifiable_nano,
            COALESCE(SUM(uh.total_cost) FILTER (WHERE NOT COALESCE(m.verifiable, false)), 0)::bigint as external_nano,
            COALESCE(SUM(uh.request_count), 0)::bigint as requests,
            (COALESCE(SUM(uh.input_tokens), 0) + COALESCE(SUM(uh.output_tokens), 0))::bigint as tokens,
            COUNT(DISTINCT uh.model_name)::bigint as models_used,
            BOOL_OR(p.organization_id IS NOT NULL) as is_paying,
            MAX(uh.last_usage_at) as last_usage_at,
            COUNT(*) OVER ()::bigint as total_groups
        FROM organizations o
        INNER JOIN usage_rows uh ON uh.organization_id = o.id
        LEFT JOIN models m ON m.id = uh.model_id
        LEFT JOIN paying p ON p.organization_id = o.id
        WHERE ($4::text IS NULL OR o.name ILIKE $4)
        GROUP BY o.id, o.name
        HAVING ($3::bool IS NULL OR BOOL_OR(p.organization_id IS NOT NULL) = $3)
    "#;

    let cte_and_from = with_usage_rows("$1", "$2", cte_and_from);
    let data_sql = format!("{cte_and_from} ORDER BY {sort_col} DESC LIMIT $5 OFFSET $6");
    // One bind list: the count shares the data query's filters ($1-$4).
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] = [
        &start,
        &end,
        &query.paying,
        &org_like,
        &query.limit,
        &query.offset,
    ];
    arm(tx, deadline).await?;
    let rows = tx.query(&data_sql, &params).await.map_err(map_db_error)?;
    let count_sql = format!("SELECT COUNT(*)::bigint FROM ({cte_and_from}) t");
    let total = pagination::page_total(
        tx,
        deadline,
        &rows,
        (query.limit, query.offset),
        &count_sql,
        &params[..4],
    )
    .await?;

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
        period_start: start,
        period_end: end,
        data,
        total,
        limit: query.limit,
        offset: query.offset,
    })
}

pub(super) async fn get_revenue_density(
    tx: &Transaction<'_>,
    deadline: Instant,
    query: RevenueDensityQuery,
) -> Result<RevenueDensityReport, RepositoryError> {
    const NANO_TO_USD: f64 = 1.0 / 1_000_000_000.0;
    // 1 minute × 60 min/h × 24 h/d × 365 d/yr
    const YEAR_MINUTES: f64 = 60.0 * 24.0 * 365.0;

    // ── Platform-wide percentiles ─────────────────────────────────────────
    // Inner CTE: one row per minute bucket with sum(cost) = nano-USD/min.
    // Outer query: PERCENTILE_CONT + MAX over active minutes only (cost > 0).
    arm(tx, deadline).await?;
    let platform_row = tx
        .query_one(
            r#"
                WITH per_minute AS (
                    SELECT
                        DATE_TRUNC('minute', ul.created_at) AS bucket,
                        SUM(ul.total_cost)::float8          AS revenue_per_min
                    FROM organization_usage_log ul
                    LEFT JOIN models m ON m.id = ul.model_id
                    WHERE ul.created_at >= $1
                      AND ul.created_at < $2
                      AND ($3::text IS NULL OR m.provider_type = $3)
                    GROUP BY 1
                )
                SELECT
                    COALESCE(
                        PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY revenue_per_min)
                            FILTER (WHERE revenue_per_min > 0), 0.0
                    )::float8 AS p50,
                    COALESCE(
                        PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY revenue_per_min)
                            FILTER (WHERE revenue_per_min > 0), 0.0
                    )::float8 AS p95,
                    COALESCE(
                        PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY revenue_per_min)
                            FILTER (WHERE revenue_per_min > 0), 0.0
                    )::float8 AS p99,
                    COALESCE(MAX(revenue_per_min), 0.0)::float8              AS peak,
                    COUNT(*) FILTER (WHERE revenue_per_min > 0)::bigint       AS active_minutes,
                    COUNT(*)::bigint                                           AS sampled_minutes
                FROM per_minute
                "#,
            &[&query.start, &query.end, &query.provider_type],
        )
        .await
        .map_err(map_db_error)?;

    let p50_nano: f64 = platform_row.get(0);
    let p95_nano: f64 = platform_row.get(1);
    let p99_nano: f64 = platform_row.get(2);
    let peak_nano: f64 = platform_row.get(3);
    let active_minutes: i64 = platform_row.get(4);
    let sampled_minutes: i64 = platform_row.get(5);

    let p50 = p50_nano * NANO_TO_USD;
    let p95 = p95_nano * NANO_TO_USD;
    let p99 = p99_nano * NANO_TO_USD;
    let peak = peak_nano * NANO_TO_USD;

    // ── Per-model percentiles ─────────────────────────────────────────────
    arm(tx, deadline).await?;
    let model_rows = tx
        .query(
            r#"
                WITH per_minute AS (
                    SELECT
                        ul.model_name,
                        DATE_TRUNC('minute', ul.created_at) AS bucket,
                        SUM(ul.total_cost)::float8          AS revenue_per_min
                    FROM organization_usage_log ul
                    LEFT JOIN models m ON m.id = ul.model_id
                    WHERE ul.created_at >= $1
                      AND ul.created_at < $2
                      AND ($3::text IS NULL OR m.provider_type = $3)
                    GROUP BY 1, 2
                )
                SELECT
                    model_name,
                    COALESCE(
                        PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY revenue_per_min)
                            FILTER (WHERE revenue_per_min > 0), 0.0
                    )::float8 AS p50,
                    COALESCE(
                        PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY revenue_per_min)
                            FILTER (WHERE revenue_per_min > 0), 0.0
                    )::float8 AS p95,
                    COALESCE(
                        PERCENTILE_CONT(0.99) WITHIN GROUP (ORDER BY revenue_per_min)
                            FILTER (WHERE revenue_per_min > 0), 0.0
                    )::float8 AS p99,
                    COALESCE(MAX(revenue_per_min), 0.0)::float8              AS peak,
                    COUNT(*) FILTER (WHERE revenue_per_min > 0)::bigint       AS active_minutes,
                    SUM(revenue_per_min)::float8                              AS total_cost_nano
                FROM per_minute
                GROUP BY model_name
                ORDER BY total_cost_nano DESC
                "#,
            &[&query.start, &query.end, &query.provider_type],
        )
        .await
        .map_err(map_db_error)?;

    let by_model: Vec<RevenueDensityModelRow> = model_rows
        .iter()
        .map(|row| {
            let model_p50 = row.get::<_, f64>(1) * NANO_TO_USD;
            let model_p95 = row.get::<_, f64>(2) * NANO_TO_USD;
            let model_p99 = row.get::<_, f64>(3) * NANO_TO_USD;
            let model_peak = row.get::<_, f64>(4) * NANO_TO_USD;
            RevenueDensityModelRow {
                model_name: row.get(0),
                p50_usd_per_min: model_p50,
                p95_usd_per_min: model_p95,
                p99_usd_per_min: model_p99,
                peak_usd_per_min: model_peak,
                p99_annualized_usd: model_p99 * YEAR_MINUTES,
                peak_annualized_usd: model_peak * YEAR_MINUTES,
                active_minutes: row.get(5),
            }
        })
        .collect();

    Ok(RevenueDensityReport {
        period_start: query.start,
        period_end: query.end,
        sampled_minutes,
        active_minutes,
        p50_usd_per_min: p50,
        p95_usd_per_min: p95,
        p99_usd_per_min: p99,
        peak_usd_per_min: peak,
        p99_annualized_usd: p99 * YEAR_MINUTES,
        peak_annualized_usd: peak * YEAR_MINUTES,
        by_model,
    })
}
