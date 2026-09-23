//! `total` on the paginated revenue reports must count every matching group,
//! on any page and under the HAVING-based `paying` filter.
use crate::admin_provider_attribution_support::{
    setup_platform_provider_usage_fixture, PlatformProviderUsageFixture,
};
use crate::common::*;
use chrono::Utc;
use database::repositories::PgAnalyticsRepository;
use services::admin::{
    AnalyticsRepository, ModelRevenueQuery, OrgRevenueQuery, OrgRevenueReport, RevenueSort,
};
use uuid::Uuid;

const USD: i64 = 1_000_000_000;

/// An organization in the tag's cohort with one usage row, created on the
/// fixture's server so the whole test shares one server and database pool.
async fn org_with_usage(
    fixture: &PlatformProviderUsageFixture,
    tag: &str,
    suffix: &str,
    cost: i64,
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
             ) VALUES ($1, $2, $3, $4, $5, 10, 10, 20, $6, 0, $6, 'chat_completion', NOW())",
            &[
                &organization_id,
                &workspace_id,
                &api_key_id,
                &fixture.model_id,
                &fixture.model_name,
                &cost,
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
    offset: i64,
) -> OrgRevenueReport {
    repository
        .get_org_revenue(OrgRevenueQuery {
            start: Utc::now() - chrono::Duration::hours(1),
            end: Utc::now() + chrono::Duration::hours(1),
            paying,
            search: Some(search.to_string()),
            sort: RevenueSort::Revenue,
            limit: 1,
            offset,
        })
        .await
        .expect("org revenue")
}

#[tokio::test]
async fn org_revenue_total_counts_matching_orgs_on_every_page() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let tag = format!("revenue-total-{}", Uuid::new_v4().simple());
    let first = org_with_usage(&fixture, &tag, "a", 3 * USD).await;
    let second = org_with_usage(&fixture, &tag, "b", 2 * USD).await;
    let third = org_with_usage(&fixture, &tag, "c", USD).await;
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
    assert_eq!(paying.data[0].organization_id, first);
    let not_paying = org_revenue(&repository, &tag, Some(false), 0).await;
    assert_eq!(not_paying.total, 2);
    assert_eq!(not_paying.data[0].organization_id, second);
    let paying_past_end = org_revenue(&repository, &tag, Some(true), 5).await;
    assert!(paying_past_end.data.is_empty());
    assert_eq!(
        paying_past_end.total, 1,
        "filtered total past the last page"
    );
}

#[tokio::test]
async fn revenue_reports_total_zero_when_nothing_matches() {
    let fixture = setup_platform_provider_usage_fixture().await;
    let repository = PgAnalyticsRepository::new(fixture.database.pool().clone());
    let nothing = format!("no-match-{}", Uuid::new_v4().simple());

    let orgs = org_revenue(&repository, &nothing, None, 0).await;
    assert!(orgs.data.is_empty());
    assert_eq!(orgs.total, 0);

    let models = repository
        .get_model_revenue(ModelRevenueQuery {
            start: Utc::now() - chrono::Duration::hours(1),
            end: Utc::now() + chrono::Duration::hours(1),
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
