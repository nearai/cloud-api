//! E2E test: web search billing is recorded correctly. Web search is mocked; we assert
//! organization balance (total_spent) increases by the configured cost per request.

mod common;

use common::*;

#[tokio::test]
async fn test_web_search_cost_recorded_correctly() {
    let (server, _database) = setup_test_server_with_mock_web_search().await;

    // Org must have a spend limit or usage middleware returns 402
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await; // $10.00 USD
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    // Get or create web_search service with known cost: 1_000_000 nano-USD per request
    let created = get_or_create_web_search_service(&server).await;
    assert_eq!(created.cost_per_unit, 1_000_000);

    // Balance before any web search
    let balance_before = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(balance_before.status_code(), 200);
    let before = balance_before.json::<api::routes::usage::OrganizationBalanceResponse>();
    assert_eq!(before.total_spent, 0, "Initial total_spent should be 0");

    // One web search request (mock returns fixed results; no Brave call)
    let search_response = server
        .get("/v1/web/search?q=test+query")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        search_response.status_code(),
        200,
        "Web search should succeed: {}",
        search_response.text()
    );

    // Allow background record_service_usage task to complete before asserting balance
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Balance after: total_spent should be 1 request * 1_000_000 nano-USD
    let balance_after = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(balance_after.status_code(), 200);
    let after = balance_after.json::<api::routes::usage::OrganizationBalanceResponse>();
    assert_eq!(
        after.total_spent, 1_000_000,
        "total_spent should be 1 * cost_per_unit (1_000_000 nano-USD)"
    );
    assert!(
        !after.total_spent_display.is_empty(),
        "total_spent_display should be set"
    );
}
