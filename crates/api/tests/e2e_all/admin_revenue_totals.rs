//! `total` on the paginated revenue reports must count every matching group,
//! on any page and under the HAVING-based `paying` filter.
use crate::admin_provider_attribution_support::{
    isolated_provider_usage_window, isolated_usage_hours, setup_platform_provider_usage_fixture,
    PlatformProviderUsageFixture,
};
use crate::common::*;
use crate::usage_hourly::{insert_raw, recompute_usage_hours};
use chrono::{DateTime, Utc};
use database::repositories::PgAnalyticsRepository;
use services::admin::{
    AnalyticsRepository, ModelRevenueQuery, OrgRevenueQuery, OrgRevenueReport, RevenueSort,
};
use uuid::Uuid;

const USD: i64 = 1_000_000_000;

/// A window around one reference instant, so seeds and queries cannot drift apart.
fn window(at: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    (
        at - chrono::Duration::hours(1),
        at + chrono::Duration::hours(1),
    )
}

/// An organization in the tag's cohort with one usage row at `at`, created on
/// the fixture's server so the whole test shares one server and database pool.
async fn org_with_usage(
    fixture: &PlatformProviderUsageFixture,
    tag: &str,
    suffix: &str,
    cost: i64,
    at: DateTime<Utc>,
) -> Uuid {
    let org = create_org(&fixture.server).await;
    let workspace = list_workspaces(&fixture.server, org.id.clone())
        .await
        .into_iter()
        .next()
        .expect("organization has a default workspace");
    let key =
        create_api_key_in_workspace(&fixture.server, workspace.id.clone(), suffix.to_string())
            .await;
    let organization_id = Uuid::parse_str(&org.id).expect("organization id");
    let workspace_id = Uuid::parse_str(&workspace.id).expect("workspace id");
    let api_key_id = Uuid::parse_str(&key.id).expect("api key id");
    let client = fixture.database.pool().get().await.expect("db connection");
    client
        .execute(
            "UPDATE organizations SET name = $2 WHERE id = $1",
            &[&organization_id, &format!("{tag}-{suffix}")],
        )
        .await
        .expect("rename organization into the cohort");
    client
        .execute(
            "INSERT INTO organization_usage_log (
                organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, total_tokens, input_cost, output_cost,
                total_cost, request_type, created_at
             ) VALUES ($1, $2, $3, $4, $5, 10, 10, 20, $6, 0, $6, 'chat_completion', $7)",
            &[
                &organization_id,
                &workspace_id,
                &api_key_id,
                &fixture.model_id,
                &fixture.model_name,
                &cost,
                &at,
            ],
        )
        .await
        .expect("insert usage");
    organization_id
}

async fn org_revenue(
    repository: &PgAnalyticsRepository,
    search: &str,
    paying: Option<bool>,
    (limit, offset): (i64, i64),
    at: DateTime<Utc>,
) -> OrgRevenueReport {
    let (start, end) = window(at);
    repository
        .get_org_revenue(OrgRevenueQuery {
            start,
            end,
            paying,
            search: Some(search.to_string()),
            sort: RevenueSort::Revenue,
            limit,
            offset,
        })
        .await
        .expect("org revenue")
}

#[tokio::test]
async fn org_revenue_total_counts_matching_orgs_on_every_page() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let tag = format!("revenue-total-{}", Uuid::new_v4().simple());
    let at = Utc::now();
    let first = org_with_usage(&fixture, &tag, "a", 3 * USD, at).await;
    let second = org_with_usage(&fixture, &tag, "b", 2 * USD, at).await;
    let third = org_with_usage(&fixture, &tag, "c", USD, at).await;
    recompute_usage_hours(
        services::usage::trunc_hour(at),
        services::usage::trunc_hour(at) + chrono::Duration::hours(1),
    )
    .await;
    fixture
        .database
        .pool()
        .get()
        .await
        .expect("db connection")
        .execute(
            "INSERT INTO organization_limits_history (organization_id, credit_type, spend_limit)
             VALUES ($1, 'payment', 1000000000000)",
            &[&first],
        )
        .await
        .expect("make the first organization paying");
    let repository = PgAnalyticsRepository::new(fixture.database.pool().clone());

    for (offset, organization_id) in [first, second, third].iter().enumerate() {
        let page = org_revenue(&repository, &tag, None, (1, offset as i64), at).await;
        assert_eq!(page.total, 3, "total on page {offset}");
        let ids: Vec<Uuid> = page.data.iter().map(|org| org.organization_id).collect();
        assert_eq!(ids, vec![*organization_id], "page {offset}");
    }
    let past_end = org_revenue(&repository, &tag, None, (1, 3), at).await;
    assert!(past_end.data.is_empty());
    assert_eq!(past_end.total, 3, "total past the last page");

    let paying = org_revenue(&repository, &tag, Some(true), (1, 0), at).await;
    assert_eq!(paying.total, 1);
    assert_eq!(paying.data[0].organization_id, first);
    let not_paying = org_revenue(&repository, &tag, Some(false), (1, 0), at).await;
    assert_eq!(not_paying.total, 2);
    assert_eq!(not_paying.data[0].organization_id, second);
    let paying_past_end = org_revenue(&repository, &tag, Some(true), (1, 5), at).await;
    assert!(paying_past_end.data.is_empty());
    assert_eq!(
        paying_past_end.total, 1,
        "filtered total past the last page"
    );

    // The routes reject limit 0, but the repository must not misreport it:
    // an empty first page is not an empty result set.
    let count_only = org_revenue(&repository, &tag, None, (0, 0), at).await;
    assert!(count_only.data.is_empty());
    assert_eq!(count_only.total, 3, "total with limit 0");
}

#[tokio::test]
async fn revenue_reports_total_zero_when_nothing_matches() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let repository = PgAnalyticsRepository::new(fixture.database.pool().clone());
    let nothing = format!("no-match-{}", Uuid::new_v4().simple());
    let at = Utc::now();

    let orgs = org_revenue(&repository, &nothing, None, (1, 0), at).await;
    assert!(orgs.data.is_empty());
    assert_eq!(orgs.total, 0);

    let (start, end) = window(at);
    let models = repository
        .get_model_revenue(ModelRevenueQuery {
            start,
            end,
            verifiable: None,
            provider_type: None,
            model_search: Some(nothing),
            sort: RevenueSort::Revenue,
            limit: 1,
            offset: 0,
        })
        .await
        .expect("model revenue");
    assert!(models.data.is_empty());
    assert_eq!(models.total, 0);
}

fn assert_close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("value present");
    assert!((actual - expected).abs() < 1e-6, "{actual} != {expected}");
}

#[tokio::test]
async fn model_and_org_revenue_serve_usage_hourly_over_whole_hours() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let tag = format!("revenue-hourly-{}", Uuid::new_v4().simple());
    fixture
        .database
        .pool()
        .get()
        .await
        .expect("db connection")
        .execute(
            "UPDATE organizations SET name = $2 WHERE id = $1",
            &[&fixture.organization_id, &tag],
        )
        .await
        .expect("rename organization");
    let (h, slot_end) = isolated_usage_hours(&fixture, 2).await;
    for (minutes, cost, ttft, provider) in [
        (10, USD, 100, "external"),
        (20, 2 * USD, 200, "external"),
        (70, 4 * USD, 1000, "chutes"),
    ] {
        insert_raw(
            &fixture,
            h + chrono::Duration::minutes(minutes),
            cost,
            10,
            Some(ttft),
            None,
            Some(provider),
        )
        .await;
    }
    recompute_usage_hours(h, slot_end).await;
    let repository = PgAnalyticsRepository::new(fixture.database.pool().clone());
    let (start, end) = (h, h + chrono::Duration::hours(2));

    let models = repository
        .get_model_revenue(ModelRevenueQuery {
            start,
            end,
            verifiable: None,
            provider_type: None,
            model_search: Some(fixture.model_name.clone()),
            sort: RevenueSort::Revenue,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("model revenue");
    assert_eq!(
        (models.period_start, models.period_end),
        (h, h + chrono::Duration::hours(2))
    );
    assert_eq!(models.total, 1);
    let entry = &models.data[0];
    assert_eq!(
        (entry.requests, entry.tokens, entry.unique_orgs),
        (3, 30, 1)
    );
    assert_eq!(entry.consumed_cost_usd, 7.0);
    // Hour h: p95 of [100, 200] = 195 over 2 samples; hour h+1: 1000 over 1 sample.
    assert_close(entry.avg_ttft_ms, 1300.0 / 3.0);
    assert_close(entry.p95_ttft_ms, 1390.0 / 3.0);
    let chutes: Vec<_> = entry
        .served_provider_breakdown
        .iter()
        .filter(|b| b.provider_type.as_deref() == Some("chutes"))
        .collect();
    assert_eq!(chutes.len(), 1);
    assert_eq!(chutes[0].requests, 1);

    let orgs = repository
        .get_org_revenue(OrgRevenueQuery {
            start,
            end,
            paying: None,
            search: Some(tag),
            sort: RevenueSort::Revenue,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("org revenue");
    assert_eq!(
        (orgs.period_start, orgs.period_end),
        (h, h + chrono::Duration::hours(2))
    );
    assert_eq!(orgs.total, 1);
    assert_eq!(orgs.data[0].organization_id, fixture.organization_id);
    assert_eq!((orgs.data[0].requests, orgs.data[0].models_used), (3, 1));
    assert_eq!(
        orgs.data[0].last_usage_at,
        Some(h + chrono::Duration::minutes(70)),
        "last usage stays an exact instant"
    );
}

/// Platform-global sums: runs under a serialized nextest override, so no other test writes
/// between the two reads.
#[tokio::test]
async fn serial_billing_summary_inference_split_reads_usage_hourly() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let (hour, end) = isolated_provider_usage_window(&fixture).await;
    insert_raw(
        &fixture,
        hour + chrono::Duration::minutes(1),
        7 * USD,
        1,
        None,
        None,
        Some("external"),
    )
    .await;
    let repository = PgAnalyticsRepository::new(fixture.database.pool().clone());

    let before = repository
        .get_billing_summary()
        .await
        .expect("billing summary");
    recompute_usage_hours(hour, end).await;
    let after = repository
        .get_billing_summary()
        .await
        .expect("billing summary");

    assert!(
        (after.inference_consumed_usd - before.inference_consumed_usd - 7.0).abs() < 1e-6,
        "inference split moves with usage_hourly: {} -> {}",
        before.inference_consumed_usd,
        after.inference_consumed_usd
    );
    assert_eq!(after.service_consumed_usd, before.service_consumed_usd);
    assert_eq!(
        after.total_consumed_usd, before.total_consumed_usd,
        "total stays on the live organization_balance (spec §6.2, §6.4)"
    );
}
