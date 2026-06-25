// E2E tests for the activation pricing gate (allow_free flag + zero-price rejection)
//
// These tests verify that:
//   1. PATCH is_active=true on a model with zero pricing returns HTTP 400
//   2. PATCH with allow_free=true + is_active=true + zero prices succeeds
//   3. PATCH with non-zero prices + is_active=true succeeds (existing behavior unaffected)

use crate::common::*;

/// Helper: create a minimal zero-priced model in the catalog (inactive).
async fn create_zero_price_model(server: &axum_test::TestServer, model_name: &str) {
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            model_name: {
                "inputCostPerToken": {"amount": 0, "currency": "USD"},
                "outputCostPerToken": {"amount": 0, "currency": "USD"},
                "modelDisplayName": "Zero Price Test Model",
                "modelDescription": "A model for pricing gate tests",
                "contextLength": 4096,
                "maxOutputLength": 1024,
                "isActive": false
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Setup: failed to create zero-price model: {}",
        response.text()
    );
}

/// Activating a zero-price model without allow_free must be rejected (HTTP 400).
#[tokio::test]
async fn test_activation_gate_rejects_zero_price() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-reject-{}", uuid::Uuid::new_v4());

    create_zero_price_model(&server, &model_name).await;

    // Attempt to activate without setting allow_free
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "isActive": true
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Activating a zero-price model without allow_free must return 400, got: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    let error_msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        error_msg.contains("zero pricing") || error_msg.contains("allowFree"),
        "Error message should mention zero pricing or allowFree, got: {error_msg}"
    );
}

/// Activating a zero-price model with allow_free=true must succeed (HTTP 200).
#[tokio::test]
async fn test_activation_gate_allows_free_with_flag() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-allow-{}", uuid::Uuid::new_v4());

    create_zero_price_model(&server, &model_name).await;

    // Activate with allow_free=true
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "isActive": true,
                "allowFree": true
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Activating a zero-price model with allow_free=true should succeed, got: {}",
        response.text()
    );
}

/// Creating a new model with non-zero pricing and is_active=true must succeed.
#[tokio::test]
async fn test_activation_gate_passes_with_nonzero_pricing() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-priced-{}", uuid::Uuid::new_v4());

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "inputCostPerToken": {"amount": 1_000_000, "currency": "USD"},
                "outputCostPerToken": {"amount": 2_000_000, "currency": "USD"},
                "modelDisplayName": "Priced Model",
                "modelDescription": "A model with real pricing",
                "contextLength": 4096,
                "maxOutputLength": 1024,
                "isActive": true
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Activating a non-zero-price model should succeed, got: {}",
        response.text()
    );
}

/// Updating a non-activation field on a zero-price model should NOT be blocked.
/// The gate only fires when the effective is_active is true.
#[tokio::test]
async fn test_activation_gate_skipped_without_is_active_true() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-skip-{}", uuid::Uuid::new_v4());

    create_zero_price_model(&server, &model_name).await;

    // Update display name without touching is_active — must not trigger the gate
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "modelDisplayName": "Updated Display Name"
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Updating a non-activation field must not trigger the gate, got: {}",
        response.text()
    );
}

/// Creating a new zero-price model without isActive omitted should be rejected.
/// When isActive is omitted on a create, the DB defaults it to true, so the gate
/// must fire even though is_active was not explicitly set in the request.
#[tokio::test]
async fn test_activation_gate_rejects_new_model_with_default_active() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-default-active-{}", uuid::Uuid::new_v4());

    // Create a new model with zero pricing and isActive omitted (defaults to true)
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "inputCostPerToken": {"amount": 0, "currency": "USD"},
                "outputCostPerToken": {"amount": 0, "currency": "USD"},
                "modelDisplayName": "Default Active Zero Price",
                "modelDescription": "Gate should fire due to DB default is_active=true",
                "contextLength": 4096,
                "maxOutputLength": 1024
                // isActive omitted — DB inserts default to true
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Creating a zero-price model without isActive should be rejected (DB defaults active=true), got: {}",
        response.text()
    );

    let body: serde_json::Value = response.json();
    let error_msg = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        error_msg.contains("zero pricing") || error_msg.contains("allowFree"),
        "Error message should mention zero pricing or allowFree, got: {error_msg}"
    );
}

/// A model with non-zero costPerImage and zero token costs should NOT be blocked.
/// The gate checks all four cost fields; non-zero image cost alone is sufficient.
#[tokio::test]
async fn test_activation_gate_allows_nonzero_image_cost() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-image-cost-{}", uuid::Uuid::new_v4());

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "inputCostPerToken": {"amount": 0, "currency": "USD"},
                "outputCostPerToken": {"amount": 0, "currency": "USD"},
                "costPerImage": {"amount": 5_000_000, "currency": "USD"},
                "modelDisplayName": "Image Only Priced Model",
                "modelDescription": "Has costPerImage; token costs are zero",
                "contextLength": 4096,
                "maxOutputLength": 1024,
                "isActive": true
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "A model with non-zero costPerImage and zero token costs should be allowed, got: {}",
        response.text()
    );
}

/// A model with non-zero cacheReadCostPerToken and zero other costs should NOT be blocked.
#[tokio::test]
async fn test_activation_gate_allows_nonzero_cache_read_cost() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-cache-cost-{}", uuid::Uuid::new_v4());

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "inputCostPerToken": {"amount": 0, "currency": "USD"},
                "outputCostPerToken": {"amount": 0, "currency": "USD"},
                "cacheReadCostPerToken": {"amount": 500_000, "currency": "USD"},
                "modelDisplayName": "Cache Read Priced Model",
                "modelDescription": "Has cacheReadCostPerToken; other costs are zero",
                "contextLength": 4096,
                "maxOutputLength": 1024,
                "isActive": true
            }
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "A model with non-zero cacheReadCostPerToken should be allowed, got: {}",
        response.text()
    );
}

/// History records must expose the allowFree flag so the audit trail reflects
/// whether free-serving was intentionally permitted at each snapshot.
#[tokio::test]
async fn test_history_includes_allow_free_flag() {
    let server = setup_test_server().await;
    let model_name = format!("test-gate-history-allow-free-{}", uuid::Uuid::new_v4());

    // Create a free model with allow_free=true
    let create_response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            &model_name: {
                "inputCostPerToken": {"amount": 0, "currency": "USD"},
                "outputCostPerToken": {"amount": 0, "currency": "USD"},
                "modelDisplayName": "Free Model With History",
                "modelDescription": "Testing allow_free in history",
                "contextLength": 4096,
                "maxOutputLength": 1024,
                "isActive": true,
                "allowFree": true
            }
        }))
        .await;

    assert_eq!(
        create_response.status_code(),
        200,
        "Creating a free model with allowFree=true should succeed, got: {}",
        create_response.text()
    );

    // Fetch the history
    let history_response = server
        .get(format!("/v1/admin/models/{model_name}/history").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        history_response.status_code(),
        200,
        "Failed to fetch model history: {}",
        history_response.text()
    );

    let history: serde_json::Value = history_response.json();
    let entries = history["history"]
        .as_array()
        .expect("history should be an array");
    assert!(
        !entries.is_empty(),
        "Should have at least one history entry"
    );

    let latest = &entries[0];
    assert_eq!(
        latest["allowFree"].as_bool(),
        Some(true),
        "Latest history entry should reflect allowFree=true"
    );
}
