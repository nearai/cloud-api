//! End-to-end tests for organization credit type tracking feature.
//!
//! Tests the ability to track different types of credits
//! with optional source tracking and multi-currency support.

use crate::common::*;

/// Test adding various credit types with different sources and currencies
#[tokio::test]
async fn test_add_credits_variations() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    // Test cases: (credit_type, source, amount, currency, expected_currency)
    let test_cases = [
        ("grant", Some("nearai"), 10_000_000_000i64, "USD", "USD"),
        ("payment", Some("stripe"), 50_000_000_000i64, "USD", "USD"),
        (
            "staking_farm",
            Some("house-of-stake"),
            20_000_000_000i64,
            "USD",
            "USD",
        ),
        (
            "postpay",
            Some("contract"),
            10_000_000_000_000_000i64,
            "USD",
            "USD",
        ),
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

/// Test balance response includes active credit source/type breakdown and that
/// a postpay ceiling participates in usage authorization without becoming a
/// prepaid balance in the UI.
#[tokio::test]
async fn test_balance_response_includes_credit_limit_breakdown() {
    let server = setup_test_server().await;
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
        Some("stripe+hot-pay"),
        50_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    add_credits_with_type(
        &server,
        &org.id,
        "staking_farm",
        Some("house-of-stake"),
        20_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;
    add_credits_with_type(
        &server,
        &org.id,
        "postpay",
        Some("contract"),
        10_000_000_000_000_000,
        "USD",
        &get_session_id(),
    )
    .await;

    let response = server
        .get(format!("/v1/organizations/{}/usage/balance", org.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);
    let balance = response.json::<api::routes::usage::OrganizationBalanceResponse>();

    assert_eq!(balance.spend_limit, Some(10_000_080_000_000_000));
    assert_eq!(balance.credit_limits.len(), 4);
    assert!(balance.credit_limits.iter().any(|limit| {
        limit.credit_type == "payment"
            && limit.source.as_deref() == Some("stripe+hot-pay")
            && limit.amount == 50_000_000_000
    }));
    assert!(balance.credit_limits.iter().any(|limit| {
        limit.credit_type == "staking_farm"
            && limit.source.as_deref() == Some("house-of-stake")
            && limit.amount == 20_000_000_000
    }));
    assert!(balance.credit_limits.iter().any(|limit| {
        limit.credit_type == "postpay"
            && limit.source.as_deref() == Some("contract")
            && limit.amount == 10_000_000_000_000_000
    }));
}
