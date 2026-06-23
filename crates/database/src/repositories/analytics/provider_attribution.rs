use super::nano_to_usd;
use chrono::{DateTime, Utc};
use services::admin::{
    ModelProviderRevenueBreakdown, ModelRevenueEntry, ModelRevenueQuery, PlatformProviderUsage,
    ProviderTierUsage, ProviderTypeUsage, ProviderUsageTotals,
};
use services::common::RepositoryError;
use std::collections::BTreeMap;

fn provider_usage_totals_from_row(
    row: &tokio_postgres::Row,
    cost_index: usize,
) -> ProviderUsageTotals {
    ProviderUsageTotals {
        requests: row.get(1),
        input_tokens: row.get(2),
        output_tokens: row.get(3),
        total_tokens: row.get(4),
        cache_read_tokens: row.get(5),
        consumed_cost_usd: nano_to_usd(row.get::<_, i64>(cost_index)),
    }
}

pub(super) async fn get_platform_provider_usage(
    client: &tokio_postgres::Client,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<PlatformProviderUsage, RepositoryError> {
    let fallback_rows = client
        .query(
            r#"
            SELECT
                served_via_fallback,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                (COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0))::bigint as total_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM organization_usage_log
            WHERE created_at >= $1 AND created_at < $2
            GROUP BY served_via_fallback
            ORDER BY served_via_fallback
            "#,
            &[&start, &end],
        )
        .await
        .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

    let mut fallback = ProviderUsageTotals::default();
    let mut non_fallback = ProviderUsageTotals::default();
    for row in &fallback_rows {
        let totals = provider_usage_totals_from_row(row, 6);
        if row.get::<_, bool>(0) {
            fallback = totals;
        } else {
            non_fallback = totals;
        }
    }

    let provider_type_rows = client
        .query(
            r#"
            SELECT
                served_provider_type,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                (COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0))::bigint as total_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM organization_usage_log
            WHERE created_at >= $1 AND created_at < $2
            GROUP BY served_provider_type
            ORDER BY served_provider_type NULLS FIRST
            "#,
            &[&start, &end],
        )
        .await
        .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

    let by_provider_type: Vec<ProviderTypeUsage> = provider_type_rows
        .iter()
        .map(|row| {
            let totals = provider_usage_totals_from_row(row, 6);
            ProviderTypeUsage {
                provider_type: row.get(0),
                requests: totals.requests,
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                total_tokens: totals.total_tokens,
                cache_read_tokens: totals.cache_read_tokens,
                consumed_cost_usd: totals.consumed_cost_usd,
            }
        })
        .collect();

    let provider_tier_rows = client
        .query(
            r#"
            SELECT
                served_provider_tier,
                COUNT(*)::bigint as requests,
                COALESCE(SUM(input_tokens), 0)::bigint as input_tokens,
                COALESCE(SUM(output_tokens), 0)::bigint as output_tokens,
                (COALESCE(SUM(input_tokens), 0) + COALESCE(SUM(output_tokens), 0))::bigint as total_tokens,
                COALESCE(SUM(cache_read_tokens), 0)::bigint as cache_read_tokens,
                COALESCE(SUM(total_cost), 0)::bigint as cost_nano
            FROM organization_usage_log
            WHERE created_at >= $1 AND created_at < $2
            GROUP BY served_provider_tier
            ORDER BY served_provider_tier NULLS FIRST
            "#,
            &[&start, &end],
        )
        .await
        .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

    let by_provider_tier: Vec<ProviderTierUsage> = provider_tier_rows
        .iter()
        .map(|row| {
            let totals = provider_usage_totals_from_row(row, 6);
            ProviderTierUsage {
                provider_tier: row.get(0),
                requests: totals.requests,
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                total_tokens: totals.total_tokens,
                cache_read_tokens: totals.cache_read_tokens,
                consumed_cost_usd: totals.consumed_cost_usd,
            }
        })
        .collect();

    Ok(PlatformProviderUsage {
        fallback,
        non_fallback,
        by_provider_type,
        by_provider_tier,
    })
}

pub(super) async fn load_model_provider_breakdowns(
    client: &tokio_postgres::Client,
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
    let breakdown_rows = client
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
        .map_err(|e| RepositoryError::DatabaseError(e.into()))?;

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
