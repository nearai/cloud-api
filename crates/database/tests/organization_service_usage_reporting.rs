use services::service_usage::ports::{ServiceUsageReportCursor, ServiceUsageReportFilters};
use uuid::Uuid;

#[path = "support/service_usage_reporting_pool.rs"]
mod service_usage_reporting_pool;
#[path = "support/service_usage_reporting_rows.rs"]
mod service_usage_reporting_rows;
#[path = "support/service_usage_reporting_support.rs"]
mod service_usage_reporting_support;

use service_usage_reporting_support::{seed_usage_fixture, test_pool, ts};

#[tokio::test]
async fn organization_service_usage_reporting_filters_and_cursor() -> anyhow::Result<()> {
    // Given: service usage rows for multiple services, workspaces, API keys, and timestamps.
    let pool = test_pool().await?;
    let fixture = seed_usage_fixture(&pool).await?;
    let repository = database::repositories::OrganizationServiceUsageRepository::new(pool);

    // When: the reporting query is narrowed to one service/workspace/API-key/date window.
    let filtered = repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: fixture.org_id,
            service_name: Some(fixture.web_search_name.clone()),
            workspace_id: Some(fixture.workspace_id),
            api_key_id: Some(fixture.api_key_id),
            start_time: Some(ts("2026-07-02T00:00:00Z")),
            end_time: Some(ts("2026-07-02T00:00:00Z")),
            limit: 10,
            ..ServiceUsageReportFilters::default()
        })
        .await?;

    // Then: only rows matching every reporting filter are returned in stable tuple order.
    let ids: Vec<Uuid> = filtered.iter().map(|row| row.id).collect();
    assert_eq!(
        ids,
        vec![fixture.same_time_high_id, fixture.same_time_low_id]
    );
    assert!(filtered
        .iter()
        .all(|row| row.service_id == fixture.web_search_id));
    assert!(filtered
        .iter()
        .all(|row| row.workspace_id == fixture.workspace_id));
    assert!(filtered
        .iter()
        .all(|row| row.api_key_id == fixture.api_key_id));
    assert!(filtered
        .iter()
        .all(|row| row.service_name == fixture.web_search_name.as_str()));

    let first_page = repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: fixture.org_id,
            service_name: Some(fixture.web_search_name.clone()),
            workspace_id: Some(fixture.workspace_id),
            api_key_id: Some(fixture.api_key_id),
            limit: 2,
            ..ServiceUsageReportFilters::default()
        })
        .await?;
    let cursor_row = first_page
        .last()
        .expect("first reporting page should contain cursor row");
    let second_page = repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: fixture.org_id,
            service_name: Some(fixture.web_search_name.clone()),
            workspace_id: Some(fixture.workspace_id),
            api_key_id: Some(fixture.api_key_id),
            cursor: Some(ServiceUsageReportCursor {
                created_at: cursor_row.created_at,
                id: cursor_row.id,
            }),
            limit: 2,
            ..ServiceUsageReportFilters::default()
        })
        .await?;
    let second_page_ids: Vec<Uuid> = second_page.iter().map(|row| row.id).collect();
    assert_eq!(
        second_page_ids,
        vec![fixture.same_time_low_id, fixture.older_id]
    );

    Ok(())
}

#[tokio::test]
async fn organization_service_usage_reporting_excludes_other_orgs() -> anyhow::Result<()> {
    // Given: another organization has a matching service usage row.
    let pool = test_pool().await?;
    let fixture = seed_usage_fixture(&pool).await?;
    let repository = database::repositories::OrganizationServiceUsageRepository::new(pool);

    // When: reporting usage is requested for the other organization with this org's filters.
    let rows = repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: fixture.other_org_id,
            workspace_id: Some(fixture.workspace_id),
            api_key_id: Some(fixture.api_key_id),
            limit: 10,
            ..ServiceUsageReportFilters::default()
        })
        .await?;

    // Then: no cross-organization rows are returned.
    assert!(rows.is_empty());

    Ok(())
}

#[tokio::test]
async fn organization_service_usage_reporting_rejects_invalid_range_and_unknown_service_is_empty(
) -> anyhow::Result<()> {
    // Given: service usage exists for one organization.
    let pool = test_pool().await?;
    let fixture = seed_usage_fixture(&pool).await?;
    let repository = database::repositories::OrganizationServiceUsageRepository::new(pool);

    // When: an invalid date range and an unknown service name are queried.
    let invalid_range = repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: fixture.org_id,
            start_time: Some(ts("2026-07-04T00:00:00Z")),
            end_time: Some(ts("2026-07-03T00:00:00Z")),
            limit: 10,
            ..ServiceUsageReportFilters::default()
        })
        .await;
    let unknown_service = repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: fixture.org_id,
            service_name: Some("unknown_service".to_string()),
            limit: 10,
            ..ServiceUsageReportFilters::default()
        })
        .await?;

    // Then: invalid ranges fail closed and unknown service names do not leak other data.
    assert!(invalid_range.is_err());
    assert!(unknown_service.is_empty());

    Ok(())
}
