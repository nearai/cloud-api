use crate::common::*;

// ============================================
// API Key Creation and Management Tests
// ============================================

#[tokio::test]
async fn test_deleted_api_key_cannot_be_used() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create and delete an API key
    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key to Delete".to_string())
            .await;

    let api_key = api_key_resp.key.clone().unwrap();

    // A valid key reaches the temporary read-only Files view before deletion.
    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Valid API key should reach the temporary Files read view before deletion"
    );

    // Revoke the API key
    seeding::seed_revoke_api_key(&api_key_resp.id).await;

    // Try to use the deleted API key - should fail
    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    println!(
        "Response with deleted key: status={}",
        response.status_code()
    );
    let response_text = response.text();
    println!("Response body: {response_text}");

    assert_eq!(
        response.status_code(),
        401,
        "Deleted API key should return 401 Unauthorized"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response_text)
        .expect("Failed to parse error response");

    assert_eq!(error.error.r#type, "invalid_api_key");
    assert!(
        error.error.message.contains("Invalid") || error.error.message.contains("expired"),
        "Error message should indicate invalid or expired API key"
    );

    println!("✓ Deleted API key correctly rejected with 401");
}

// ============================================
// API Key Spend Limit Tests
// ============================================

#[tokio::test]
async fn test_api_key_spend_limit_enforcement() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD (high limit)

    // Create API key and set a very low limit
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Test API Key".to_string())
            .await;
    let api_key = api_key_resp.key.clone().unwrap();

    // Set API key spend limit to a very low amount (1 nano-dollar)
    seeding::seed_api_key_spend_limit(&api_key_resp.id, Some(1)).await;

    let model_name = setup_qwen_model(&server).await;

    // First request might succeed or fail depending on timing
    let _response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    // Wait for usage to be recorded
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Second request should fail with API key limit exceeded
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hi again"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    println!("Second request status: {}", response2.status_code());

    assert_eq!(
        response2.status_code(),
        402,
        "Expected 402 Payment Required for API key limit exceeded"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response2.text())
        .expect("Failed to parse error response");

    assert_eq!(
        error.error.r#type, "api_key_limit_exceeded",
        "Error type should be api_key_limit_exceeded"
    );
    assert!(
        error.error.message.contains("API key spend limit exceeded"),
        "Error message should mention API key limit"
    );

    println!("✓ API key spend limit correctly enforced");
}

// ============================================
// API Key Usage Tracking Tests
// ============================================

#[tokio::test]
async fn test_api_key_usage_isolated_between_keys() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create two API keys
    let api_key_resp1 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 1".to_string()).await;
    let api_key1 = api_key_resp1.key.clone().unwrap();

    let api_key_resp2 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 2".to_string()).await;
    let api_key2 = api_key_resp2.key.clone().unwrap();

    let model_name = setup_qwen_model(&server).await;

    // Make request with first key
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key1}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "First key request"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    assert_eq!(response1.status_code(), 200);

    // Make request with second key
    let response2 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {api_key2}"))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Second key request with more tokens"}],
            "stream": false,
            "max_tokens": 15
        }))
        .await;

    assert_eq!(response2.status_code(), 200);

    // Wait for usage recording
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
    crate::usage_hourly::recompute_recent_usage().await;

    // Each key has its own recorded usage
    let usage1 = seeding::api_key_spend(&api_key_resp1.id).await;
    let usage2 = seeding::api_key_spend(&api_key_resp2.id).await;

    // Both should have usage
    assert!(usage1 > 0, "Key 1 should have usage");
    assert!(usage2 > 0, "Key 2 should have usage");

    // Usage should be different (because different requests)
}

// ============================================
// API Key Authorization Tests
// ============================================

#[tokio::test]
async fn test_api_key_authentication() {
    let server = setup_test_server().await;

    let (api_key, _) = create_org_and_api_key(&server).await;

    // A valid API key reaches the temporary read-only Files view.
    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Valid API key should reach the temporary Files read view"
    );

    // Test invalid API key
    let response = server
        .get("/v1/files?limit=1")
        .add_header("Authorization", "Bearer invalid_key_12345")
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Invalid API key should return 401"
    );

    // Test missing API key
    let response = server.get("/v1/files?limit=1").await;

    assert_eq!(
        response.status_code(),
        401,
        "Missing API key should return 401"
    );

    println!("✓ API key authentication working correctly");
}
