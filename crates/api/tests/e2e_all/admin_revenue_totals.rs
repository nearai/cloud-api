//! `total` on the paginated revenue reports must count every matching group,
//! on any page and under the HAVING-based `paying` filter.
use crate::admin_provider_attribution_support::{
    insert_platform_provider_usage_row, setup_platform_provider_usage_fixture,
    PlatformProviderUsageFixture, ProviderUsageSeedRow,
};
use chrono::Utc;
use database::repositories::PgAnalyticsRepository;
use services::admin::{AnalyticsRepository, OrgRevenueQuery, OrgRevenueReport, RevenueSort};
use uuid::Uuid;

const USD: i64 = 1_000_000_000;

async fn org_with_usage(tag: &str, suffix: &str, cost: i64) -> PlatformProviderUsageFixture {
    let fixture = setup_platform_provider_usage_fixture().await;
    let client = fixture.database.pool().get().await.expect("db connection");
    client
        .execute(
            "UPDATE organizations SET name = $2 WHERE id = $1",
            &[&fixture.organization_id, &format!("{tag}-{suffix}")],
        )
        .await
        .expect("rename organization into the cohort");
    drop(client);
    insert_platform_provider_usage_row(
        &fixture,
        ProviderUsageSeedRow {
            created_at: Utc::now(),
            input_tokens: 10,
            output_tokens: 10,
            cache_read_tokens: 0,
            total_cost: cost,
            served_provider_type: None,
            served_provider_tier: None,
            served_via_fallback: false,
        },
    )
    .await;
    fixture
}

async fn org_revenue(
    repository: &PgAnalyticsRepository,
    tag: &str,
    paying: Option<bool>,
    offset: i64,
) -> OrgRevenueReport {
    repository
        .get_org_revenue(OrgRevenueQuery {
            start: Utc::now() - chrono::Duration::hours(1),
            end: Utc::now() + chrono::Duration::hours(1),
            paying,
            search: Some(tag.to_string()),
            sort: RevenueSort::Revenue,
            limit: 1,
            offset,
        })
        .await
        .expect("org revenue")
}

#[tokio::test]
async fn org_revenue_total_counts_matching_orgs_on_every_page() {
    let tag = format!("revenue-total-{}", Uuid::new_v4().simple());
    let first = org_with_usage(&tag, "a", 3 * USD).await;
    let second = org_with_usage(&tag, "b", 2 * USD).await;
    let third = org_with_usage(&tag, "c", USD).await;
    first
        .database
        .pool()
        .get()
        .await
        .expect("db connection")
        .execute(
            "INSERT INTO organization_limits_history (organization_id, credit_type, spend_limit)
             VALUES ($1, 'payment', 1000000000000)",
            &[&first.organization_id],
        )
        .await
        .expect("make the first organization paying");
    let repository = PgAnalyticsRepository::new(first.database.pool().clone());

    let expected_order = [
        first.organization_id,
        second.organization_id,
        third.organization_id,
    ];
    for (offset, organization_id) in expected_order.iter().enumerate() {
        let page = org_revenue(&repository, &tag, None, offset as i64).await;
        assert_eq!(page.total, 3, "total on page {offset}");
        let ids: Vec<Uuid> = page.data.iter().map(|org| org.organization_id).collect();
        assert_eq!(ids, vec![*organization_id], "page {offset}");
    }
    let past_end = org_revenue(&repository, &tag, None, 3).await;
    assert!(past_end.data.is_empty());
    assert_eq!(past_end.total, 3, "total past the last page");

    let paying = org_revenue(&repository, &tag, Some(true), 0).await;
    assert_eq!(paying.total, 1);
    assert_eq!(paying.data[0].organization_id, first.organization_id);
    let not_paying = org_revenue(&repository, &tag, Some(false), 0).await;
    assert_eq!(not_paying.total, 2);
    assert_eq!(not_paying.data[0].organization_id, second.organization_id);
    let paying_past_end = org_revenue(&repository, &tag, Some(true), 5).await;
    assert!(paying_past_end.data.is_empty());
    assert_eq!(
        paying_past_end.total, 1,
        "filtered total past the last page"
    );
}
