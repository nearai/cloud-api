use super::arm;
use super::nano_to_usd;
use crate::repositories::utils::map_db_error;
use chrono::{DateTime, Utc};
use services::admin::{
    ModelProviderRevenueBreakdown, ModelRevenueEntry, ModelRevenueQuery, PlatformProviderUsage,
    ProviderTierUsage, ProviderTypeUsage, ProviderUsageTotals,
};
use services::common::RepositoryError;
use std::collections::BTreeMap;
use std::time::Instant;
use tokio_postgres::Transaction;

fn provider_usage_totals_from_row(row: &tokio_postgres::Row) -> ProviderUsageTotals {
    ProviderUsageTotals {
        requests: row.get("requests"),
        input_tokens: row.get("input_tokens"),
        output_tokens: row.get("output_tokens"),
        total_tokens: row.get("total_tokens"),
        cache_read_tokens: row.get("cache_read_tokens"),
        consumed_cost_usd: nano_to_usd(row.get::<_, i64>("cost_nano")),
    }
}

// `GROUPING(fallback, type, tier)` bitmask: a set bit is a column aggregated away,
// so a real NULL provider type/tier is never confused with "not grouped".
const BY_FALLBACK: i32 = 0b011;
const BY_PROVIDER_TYPE: i32 = 0b101;
const BY_PROVIDER_TIER: i32 = 0b110;

pub(super) async fn get_platform_provider_usage(
    tx: &Transaction<'_>,
    deadline: Instant,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<PlatformProviderUsage, RepositoryError> {
    arm(tx, deadline).await?;
    let rows = tx
        .query(
            r#"
            SELECT
                GROUPING(served_via_fallback, served_provider_type, served_provider_tier) as grouping_set,
                served_via_fallback,
                served_provider_type,
                served_provider_tier,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                (COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0))::bigint as total_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM organization_usage_log
            WHERE created_at >= $1 AND created_at < $2
            GROUP BY GROUPING SETS (
                (served_via_fallback),
                (served_provider_type),
                (served_provider_tier)
            )
            ORDER BY
                served_via_fallback,
                served_provider_type NULLS FIRST,
                served_provider_tier NULLS FIRST
            "#,
            &[&start, &end],
        )
        .await
        .map_err(map_db_error)?;

    let mut usage = PlatformProviderUsage {
        fallback: ProviderUsageTotals::default(),
        non_fallback: ProviderUsageTotals::default(),
        by_provider_type: Vec::new(),
        by_provider_tier: Vec::new(),
    };
    for row in &rows {
        let totals = provider_usage_totals_from_row(row);
        match row.get::<_, i32>("grouping_set") {
            BY_FALLBACK => {
                if row.get::<_, bool>("served_via_fallback") {
                    usage.fallback = totals;
                } else {
                    usage.non_fallback = totals;
                }
            }
            BY_PROVIDER_TYPE => usage.by_provider_type.push(ProviderTypeUsage {
                provider_type: row.get("served_provider_type"),
                requests: totals.requests,
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                total_tokens: totals.total_tokens,
                cache_read_tokens: totals.cache_read_tokens,
                consumed_cost_usd: totals.consumed_cost_usd,
            }),
            BY_PROVIDER_TIER => usage.by_provider_tier.push(ProviderTierUsage {
                provider_tier: row.get("served_provider_tier"),
                requests: totals.requests,
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                total_tokens: totals.total_tokens,
                cache_read_tokens: totals.cache_read_tokens,
                consumed_cost_usd: totals.consumed_cost_usd,
            }),
            other => {
                return Err(RepositoryError::DatabaseError(anyhow::anyhow!(
                    "unexpected provider usage grouping set {other}"
                )))
            }
        }
    }
    Ok(usage)
}

pub(super) async fn load_model_provider_breakdowns(
    tx: &Transaction<'_>,
    deadline: Instant,
    query: &ModelRevenueQuery,
    where_clause: &str,
    model_like: &Option<String>,
    data: &mut [ModelRevenueEntry],
) -> Result<(), RepositoryError> {
    if data.is_empty() {
        return Ok(());
    }

    let model_names: Vec<String> = data.iter().map(|entry| entry.model_name.clone()).collect();
    let breakdown_sql = format!(
        r#"
        SELECT
            ul.model_name,
            ul.served_provider_type,
            ul.served_provider_tier,
            ul.served_via_fallback,
            COUNT(*)::bigint as requests,
            (COALESCE(SUM(ul.input_tokens), 0) + COALESCE(SUM(ul.output_tokens), 0))::bigint as tokens,
            COALESCE(SUM(ul.total_cost), 0)::bigint as cost_nano
        FROM organization_usage_log ul
        LEFT JOIN models m ON m.id = ul.model_id
        {where_clause}
          AND ul.model_name = ANY($6)
        GROUP BY ul.model_name, ul.served_provider_type, ul.served_provider_tier, ul.served_via_fallback
        ORDER BY ul.model_name, ul.served_provider_type NULLS FIRST, ul.served_provider_tier NULLS FIRST, ul.served_via_fallback
        "#
    );
    arm(tx, deadline).await?;
    let breakdown_rows = tx
        .query(
            &breakdown_sql,
            &[
                &query.start,
                &query.end,
                &query.verifiable,
                &query.provider_type,
                model_like,
                &model_names,
            ],
        )
        .await
        .map_err(map_db_error)?;

    let mut breakdowns_by_model: BTreeMap<String, Vec<ModelProviderRevenueBreakdown>> =
        BTreeMap::new();
    for row in breakdown_rows {
        let model_name: String = row.get(0);
        breakdowns_by_model
            .entry(model_name)
            .or_default()
            .push(ModelProviderRevenueBreakdown {
                provider_type: row.get(1),
                provider_tier: row.get(2),
                served_via_fallback: row.get(3),
                requests: row.get(4),
                tokens: row.get(5),
                consumed_cost_usd: nano_to_usd(row.get::<_, i64>(6)),
            });
    }

    for entry in data {
        if let Some(breakdowns) = breakdowns_by_model.remove(&entry.model_name) {
            entry.served_provider_breakdown = breakdowns;
        }
    }

    Ok(())
}
