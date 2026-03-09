//! E2E tests for service-usage history endpoint.

mod common;

use common::*;

/// After web_search calls, service-usage history for the org should contain
/// corresponding entries with correct quantity and total_cost.
#[tokio::test]
async fn test_service_usage_history_web_search() {
    let (server, _database) = setup_test_server_with_mock_web_search().await;

    // Org must have a spend limit or usage middleware returns 402
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Create web_search service with known cost: 1_000_000 nano-USD per request
    let created = create_web_search_service(&server).await;
    assert_eq!(created.cost_per_unit, 1_000_000);

    // Service-usage history before any web search should be empty
    let history_before = server
        .get(
            format!(
                "/v1/organizations/{}/service-usage/history?limit=10",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        history_before.status_code(),
        200,
        "service-usage history before calls should succeed: {}",
        history_before.text()
    );
    let before = history_before.json::<api::routes::usage::ServiceUsageHistoryResponse>();
    assert!(
        before.data.is_empty(),
        "Service-usage history should be empty before any web_search"
    );

    // Two web search requests (mock returns fixed results; no Brave call)
    for _ in 0..2 {
        let search_response = server
            .get("/v1/web/search?q=test+query")
            .add_header("Authorization", format!("Bearer {}", api_key.clone()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        assert_eq!(
            search_response.status_code(),
            200,
            "Web search should succeed: {}",
            search_response.text()
        );
    }

    // Allow background record_service_usage task to complete before asserting history
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Unfiltered service-usage history
    let history_after = server
        .get(
            format!(
                "/v1/organizations/{}/service-usage/history?limit=10",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        history_after.status_code(),
        200,
        "service-usage history after calls should succeed: {}",
        history_after.text()
    );
    let after = history_after.json::<api::routes::usage::ServiceUsageHistoryResponse>();
    assert!(
        !after.data.is_empty(),
        "Service-usage history should have at least one entry after web_search"
    );
    assert_eq!(after.limit, 10);

    // Entries are ordered by created_at DESC, so the newest call is first.
    let entry = &after.data[0];
    assert_eq!(
        entry.organization_id, org.id,
        "Entry organization_id should match org"
    );
    // Each call records quantity = 1 and total_cost = cost_per_unit
    assert_eq!(entry.quantity, 1);
    assert_eq!(
        entry.total_cost, created.cost_per_unit,
        "total_cost should equal cost_per_unit for a single request"
    );
    assert!(
        !entry.total_cost_display.is_empty(),
        "total_cost_display should be set"
    );

    // Filtered by serviceName=web_search should still return entries
    let filtered = server
        .get(
            format!(
                "/v1/organizations/{}/service-usage/history?limit=10&serviceName=web_search",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        filtered.status_code(),
        200,
        "filtered service-usage history should succeed: {}",
        filtered.text()
    );
    let filtered_body = filtered.json::<api::routes::usage::ServiceUsageHistoryResponse>();
    assert!(
        !filtered_body.data.is_empty(),
        "Filtered history by serviceName=web_search should not be empty"
    );
}
