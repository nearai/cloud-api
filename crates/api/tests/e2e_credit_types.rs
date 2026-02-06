//! End-to-end tests for organization credit type tracking feature.
//!
//! Tests the ability to track different types of credits (grant vs payment)
//! with optional source tracking and multi-currency support.

mod common;

use common::*;

/// Helper to get limits history for an organization
async fn get_limits_history(
    server: &axum_test::TestServer,
    org_id: &str,
) -> api::models::OrgLimitsHistoryResponse {
    let response = server
        .get(format!("/v1/admin/organizations/{}/limits/history", org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<api::models::OrgLimitsHistoryResponse>()
}

/// Helper to send a raw limits update request (for error cases)
async fn update_limits_raw(
    server: &axum_test::TestServer,
    org_id: &str,
    request: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .patch(format!("/v1/admin/organizations/{}/limits", org_id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&request)
        .await
}

/// Test adding various credit types with different sources and currencies
#[tokio::test]
async fn test_add_credits_variations() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Test cases: (credit_type, source, amount, currency, expected_currency)
    let test_cases = [
        ("grant", Some("nearai"), 10_000_000_000i64, "USD", "USD"),
        ("payment", Some("stripe"), 50_000_000_000i64, "USD", "USD"),
        (
            "payment",
            Some("hot-pay"),
            100_000_000_000i64,
            "USDT",
            "USDT",
        ),
        ("payment", None, 25_000_000_000i64, "USD", "USD"),
        ("grant", Some("nearai"), 15_000_000_000i64, "usd", "USD"), // lowercase currency
    ];

    for (credit_type, source, amount, currency, expected_currency) in test_cases {
        let response = add_credits_with_type(
            &server,
            &org.id,
            credit_type,
            source,
            amount,
            currency,
            &get_session_id(),
        )
        .await;

        assert_eq!(response.credit_type.as_str(), credit_type);
        assert_eq!(response.source.as_deref(), source);
        assert_eq!(response.spend_limit.amount, amount);
        assert_eq!(response.spend_limit.currency, expected_currency);
        assert_eq!(response.spend_limit.scale, 9);
    }
}

/// Test that credits accumulate across different types
#[tokio::test]
async fn test_credits_accumulate_across_types() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Add grant ($10) and payment ($50)
    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        10_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    add_credits_with_type(
        &server,
        &org.id,
        "payment",
        Some("stripe"),
        50_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;

    let history = get_limits_history(&server, &org.id).await;
    assert_eq!(history.history.len(), 2);

    let types: Vec<&str> = history
        .history
        .iter()
        .map(|h| h.credit_type.as_str())
        .collect();
    assert!(types.contains(&"grant"));
    assert!(types.contains(&"payment"));
}

/// Test that updating the same credit type replaces the previous one
#[tokio::test]
async fn test_update_same_type_replaces_previous() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Add initial grant ($10), then update to $20
    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        10_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        20_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;

    let history = get_limits_history(&server, &org.id).await;
    assert_eq!(history.history.len(), 2);

    // Most recent (first) should be active with $20, older (second) should be closed with $10
    assert!(history.history[0].effective_until.is_none());
    assert_eq!(history.history[0].spend_limit.amount, 20_000_000_000);
    assert!(history.history[1].effective_until.is_some());
    assert_eq!(history.history[1].spend_limit.amount, 10_000_000_000);
}

/// Test that different credit types can coexist and be updated independently
#[tokio::test]
async fn test_independent_credit_type_updates() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    // Add grant ($10) and payment ($50), then update grant to $15
    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        10_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    add_credits_with_type(
        &server,
        &org.id,
        "payment",
        Some("stripe"),
        50_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        15_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;

    let history = get_limits_history(&server, &org.id).await;

    // Should have 3 entries: 2 grants (1 active, 1 closed) + 1 payment (active)
    assert_eq!(history.history.len(), 3);

    let active: Vec<_> = history
        .history
        .iter()
        .filter(|h| h.effective_until.is_none())
        .collect();
    assert_eq!(active.len(), 2);

    let active_grant = active
        .iter()
        .find(|h| h.credit_type.as_str() == "grant")
        .unwrap();
    let active_payment = active
        .iter()
        .find(|h| h.credit_type.as_str() == "payment")
        .unwrap();
    assert_eq!(active_grant.spend_limit.amount, 15_000_000_000);
    assert_eq!(active_payment.spend_limit.amount, 50_000_000_000);
}

/// Test that history entries include credit type and source
#[tokio::test]
async fn test_history_includes_credit_type_and_source() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    add_credits_with_type(
        &server,
        &org.id,
        "grant",
        Some("nearai"),
        10_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    add_credits_with_type(
        &server,
        &org.id,
        "payment",
        Some("stripe"),
        50_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;

    let history = get_limits_history(&server, &org.id).await;

    let grant = history
        .history
        .iter()
        .find(|h| h.credit_type.as_str() == "grant")
        .unwrap();
    let payment = history
        .history
        .iter()
        .find(|h| h.credit_type.as_str() == "payment")
        .unwrap();

    assert_eq!(grant.source.as_deref(), Some("nearai"));
    assert_eq!(payment.source.as_deref(), Some("stripe"));
}

/// Test invalid credit type returns 400 error
#[tokio::test]
async fn test_invalid_credit_type_returns_error() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    let response = update_limits_raw(
        &server,
        &org.id,
        serde_json::json!({
            "type": "invalid_type",
            "spendLimit": { "amount": 10_000_000_000i64, "currency": "USD" },
            "changedBy": "admin@test.com",
            "changeReason": "Test invalid type"
        }),
    )
    .await;

    // Serde deserialization errors return 422 Unprocessable Entity
    assert_eq!(response.status_code(), 422);
}

/// Test credit type case insensitivity (GRANT -> grant)
#[tokio::test]
async fn test_credit_type_case_insensitive() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    let response = update_limits_raw(
        &server,
        &org.id,
        serde_json::json!({
            "type": "GRANT",
            "spendLimit": { "amount": 10_000_000_000i64, "currency": "USD" },
            "changedBy": "admin@test.com",
            "changeReason": "Test case insensitive"
        }),
    )
    .await;

    assert_eq!(response.status_code(), 200);
    assert_eq!(
        response
            .json::<api::models::UpdateOrganizationLimitsResponse>()
            .credit_type
            .as_str(),
        "grant"
    );
}

/// Test missing type field returns error (type is required)
#[tokio::test]
async fn test_missing_type_field_returns_error() {
    let (server, _guard) = setup_test_server().await;
    let org = create_org(&server).await;

    let response = update_limits_raw(
        &server,
        &org.id,
        serde_json::json!({
            "spendLimit": { "amount": 10_000_000_000i64, "currency": "USD" },
            "changedBy": "admin@test.com",
            "changeReason": "Test missing type"
        }),
    )
    .await;

    assert!(response.status_code() == 400 || response.status_code() == 422);
}
