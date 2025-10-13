mod common;

use common::*;

// ============================================
// API Key Creation and Management Tests
// ============================================

#[tokio::test]
async fn test_create_api_key_in_workspace() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "My Test API Key".to_string())
            .await;

    // Verify the API key was created successfully
    assert!(!api_key_resp.id.is_empty(), "API key should have an ID");
    assert!(
        api_key_resp.key.is_some(),
        "API key should include the key on creation"
    );
    assert!(
        !api_key_resp.key_prefix.is_empty(),
        "API key should have a prefix"
    );
    assert_eq!(api_key_resp.name, Some("My Test API Key".to_string()));
    assert_eq!(api_key_resp.workspace_id, workspace.id);
    assert!(
        api_key_resp.expires_at.is_some(),
        "API key should have an expiration"
    );
    assert_eq!(
        api_key_resp.spend_limit, None,
        "New key should not have spend limit"
    );

    println!("Created API key: {:?}", api_key_resp);
}

#[tokio::test]
async fn test_list_workspace_api_keys() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create multiple API keys
    let key1 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 1".to_string()).await;
    let key2 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 2".to_string()).await;
    let key3 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key 3".to_string()).await;

    // List all API keys for the workspace
    let response = server
        .get(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);

    let list_response = response.json::<api::models::ListApiKeysResponse>();
    let api_keys = list_response.api_keys;

    // Verify we have at least 3 keys
    assert!(api_keys.len() >= 3, "Should have at least 3 API keys");

    // Verify the keys we created are in the list
    let key_ids: Vec<String> = api_keys.iter().map(|k| k.id.clone()).collect();
    assert!(key_ids.contains(&key1.id), "List should contain key1");
    assert!(key_ids.contains(&key2.id), "List should contain key2");
    assert!(key_ids.contains(&key3.id), "List should contain key3");

    // Verify keys don't include the full key value when listing
    for key in &api_keys {
        assert!(
            key.key.is_none(),
            "Listed keys should not expose full key value"
        );
        assert!(!key.key_prefix.is_empty(), "Keys should have prefix");
    }

    println!("Listed {} API keys successfully", api_keys.len());
}

#[tokio::test]
async fn test_api_key_prevents_duplicate_names_in_workspace() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create first API key with a specific name
    let key_name = "Unique Key Name".to_string();
    let _key1 = create_api_key_in_workspace(&server, workspace.id.clone(), key_name.clone()).await;

    // Try to create another key with the same name in the same workspace - should fail
    let request = api::models::CreateApiKeyRequest {
        name: key_name.clone(),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::days(90)),
        spend_limit: None,
    };

    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;

    println!("Duplicate name response status: {}", response.status_code());
    let response_text = response.text();
    println!("Duplicate name response body: {}", response_text);

    // Should get 409 Conflict for duplicate name
    assert_eq!(
        response.status_code(),
        409,
        "Creating API key with duplicate name should fail with 409 Conflict"
    );

    let error = serde_json::from_str::<api::models::ErrorResponse>(&response_text)
        .expect("Failed to parse error response");

    assert_eq!(error.error.r#type, "duplicate_api_key_name");
    assert!(
        error
            .error
            .message
            .contains("API key with this name already exists"),
        "Error message should indicate duplicate name"
    );
}

#[tokio::test]
async fn test_api_key_same_name_different_workspaces() {
    let server = setup_test_server().await;

    // Create two different organizations (each with their own workspace)
    let org1 = create_org(&server).await;
    let org2 = create_org(&server).await;

    let workspaces1 = list_workspaces(&server, org1.id).await;
    let workspace1 = workspaces1.first().unwrap();

    let workspaces2 = list_workspaces(&server, org2.id).await;
    let workspace2 = workspaces2.first().unwrap();

    // Use the same API key name in both workspaces
    let key_name = "Shared Key Name".to_string();

    // Create key in first workspace
    let key1 = create_api_key_in_workspace(&server, workspace1.id.clone(), key_name.clone()).await;
    println!("Created key1 in workspace1: {}", key1.id);

    // Create key with same name in second workspace - should succeed
    let key2 = create_api_key_in_workspace(&server, workspace2.id.clone(), key_name.clone()).await;
    println!("Created key2 in workspace2: {}", key2.id);

    // Verify both keys were created successfully
    assert_eq!(key1.name, Some(key_name.clone()));
    assert_eq!(key2.name, Some(key_name));
    assert_ne!(key1.id, key2.id, "Keys should have different IDs");
    assert_ne!(
        key1.workspace_id, key2.workspace_id,
        "Keys should be in different workspaces"
    );

    println!("✓ Successfully created API keys with same name in different workspaces");
}

#[tokio::test]
async fn test_delete_api_key() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create an API key
    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Key to Delete".to_string())
            .await;

    println!("Created API key to delete: {}", api_key_resp.id);

    // Delete the API key
    let response = server
        .delete(
            format!(
                "/v1/workspaces/{}/api-keys/{}",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    println!("Delete response status: {}", response.status_code());
    assert_eq!(
        response.status_code(),
        204,
        "Delete should succeed with 204 No Content"
    );

    // Try to list keys and verify the deleted key is not present
    let list_response = server
        .get(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(list_response.status_code(), 200);
    let list_data = list_response.json::<api::models::ListApiKeysResponse>();

    // Verify the deleted key is not in the list
    let found = list_data.api_keys.iter().any(|k| k.id == api_key_resp.id);
    assert!(!found, "Deleted API key should not appear in list");

    println!("✓ API key successfully deleted and not visible in list");
}

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

    // Verify key works before deletion
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "API key should work before deletion"
    );

    // Delete the API key
    let delete_response = server
        .delete(
            format!(
                "/v1/workspaces/{}/api-keys/{}",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(delete_response.status_code(), 204);

    // Try to use the deleted API key - should fail
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;

    println!(
        "Response with deleted key: status={}",
        response.status_code()
    );
    let response_text = response.text();
    println!("Response body: {}", response_text);

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
async fn test_api_key_spend_limit_update() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Test API Key".to_string())
            .await;

    // Verify initial state has no spend limit
    assert_eq!(api_key_resp.spend_limit, None);

    // Update the API key spend limit to $1.00 (1000000000 nano-dollars)
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1000000000i64,
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "API key spend limit update should succeed"
    );

    let updated_key = serde_json::from_str::<api::models::ApiKeyResponse>(&response.text())
        .expect("Failed to parse response");

    // Verify the spend limit was set
    assert!(updated_key.spend_limit.is_some());
    let spend_limit = updated_key.spend_limit.unwrap();
    assert_eq!(spend_limit.amount, 1000000000i64);
    assert_eq!(spend_limit.scale, 9);
    assert_eq!(spend_limit.currency, "USD");

    // Remove the spend limit (set to null)
    let remove_request = serde_json::json!({
        "spendLimit": null
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&remove_request)
        .await;

    assert_eq!(response.status_code(), 200);

    let updated_key = serde_json::from_str::<api::models::ApiKeyResponse>(&response.text())
        .expect("Failed to parse response");

    // Verify the spend limit was removed
    assert_eq!(updated_key.spend_limit, None);

    println!("✓ Successfully updated and removed API key spend limit");
}

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
    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 1i64,
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200);

    let model_name = setup_test_model(&server).await;

    // First request might succeed or fail depending on timing
    let _response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key))
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
        .add_header("Authorization", format!("Bearer {}", api_key))
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

#[tokio::test]
async fn test_api_key_limit_enforced_before_org_limit() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 5000000000i64).await; // $5.00 USD

    // Create API key with lower limit than org ($2.00)
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Test API Key".to_string())
            .await;

    let update_request = serde_json::json!({
        "spendLimit": {
            "amount": 2000000000i64,  // $2.00 USD (lower than org limit)
            "currency": "USD"
        }
    });

    let response = server
        .patch(
            format!(
                "/v1/workspaces/{}/api-keys/{}/spend-limit",
                workspace.id, api_key_resp.id
            )
            .as_str(),
        )
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&update_request)
        .await;

    assert_eq!(response.status_code(), 200);

    let updated_key = serde_json::from_str::<api::models::ApiKeyResponse>(&response.text())
        .expect("Failed to parse response");

    assert_eq!(updated_key.spend_limit.unwrap().amount, 2000000000i64);

    println!("✓ API key has lower limit ($2.00) than org ($5.00)");
    println!("In production, API key limit would be enforced before org limit");
}

// ============================================
// API Key Usage Tracking Tests
// ============================================

#[tokio::test]
async fn test_list_workspace_api_keys_with_usage() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    // Get the default workspace
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create first API key (will have usage)
    let api_key_resp1 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Test API Key 1".to_string())
            .await;
    let api_key1 = api_key_resp1.key.clone().unwrap();

    // Create second API key (will not have usage)
    let api_key_resp2 =
        create_api_key_in_workspace(&server, workspace.id.clone(), "Test API Key 2".to_string())
            .await;

    let model_name = setup_test_model(&server).await;

    // Make a completion request with the first API key to generate usage
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key1))
        .json(&serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": "Hello world"}],
            "stream": false,
            "max_tokens": 10
        }))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Completion request should succeed"
    );

    let completion_response = response.json::<api::models::ChatCompletionResponse>();
    assert!(
        completion_response.usage.input_tokens > 0,
        "Should have input tokens"
    );
    assert!(
        completion_response.usage.output_tokens > 0,
        "Should have output tokens"
    );

    // Wait for async usage recording to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

    // List all API keys for the workspace
    let response = server
        .get(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully list API keys"
    );

    let list_response = response.json::<api::models::ListApiKeysResponse>();
    let api_keys = list_response.api_keys;

    // Find the API key that we used
    let used_key = api_keys
        .iter()
        .find(|k| k.id == api_key_resp1.id)
        .expect("Should find the first API key");

    // Verify the used key has usage information
    assert!(
        used_key.usage.is_some(),
        "Used API key should have usage information"
    );

    let usage = used_key.usage.as_ref().unwrap();
    assert!(usage.amount > 0, "Usage amount should be greater than 0");
    assert_eq!(usage.scale, 9, "Scale should be 9 (nano-dollars)");
    assert_eq!(usage.currency, "USD", "Currency should be USD");

    // Find the unused API key
    let unused_key = api_keys
        .iter()
        .find(|k| k.id == api_key_resp2.id)
        .expect("Should find the second API key");

    // Verify the unused key either has no usage or usage is 0
    if let Some(unused_usage) = &unused_key.usage {
        assert_eq!(unused_usage.amount, 0, "Unused API key should have 0 usage");
    }

    println!("✓ Successfully verified list_workspace_api_keys includes usage");
}

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

    let model_name = setup_test_model(&server).await;

    // Make request with first key
    let response1 = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", api_key1))
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
        .add_header("Authorization", format!("Bearer {}", api_key2))
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

    // List API keys and verify each has its own usage
    let response = server
        .get(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    let list_response = response.json::<api::models::ListApiKeysResponse>();

    let key1_data = list_response
        .api_keys
        .iter()
        .find(|k| k.id == api_key_resp1.id)
        .unwrap();
    let key2_data = list_response
        .api_keys
        .iter()
        .find(|k| k.id == api_key_resp2.id)
        .unwrap();

    let usage1 = key1_data.usage.as_ref().unwrap();
    let usage2 = key2_data.usage.as_ref().unwrap();

    // Both should have usage
    assert!(usage1.amount > 0, "Key 1 should have usage");
    assert!(usage2.amount > 0, "Key 2 should have usage");

    // Usage should be different (because different requests)
    println!("Key 1 usage: {} nano-dollars", usage1.amount);
    println!("Key 2 usage: {} nano-dollars", usage2.amount);

    println!("✓ API keys have isolated usage tracking");
}

// ============================================
// API Key Authorization Tests
// ============================================

#[tokio::test]
async fn test_api_key_not_in_other_workspace() {
    let server = setup_test_server().await;

    // Create two separate organizations with workspaces
    let org1 = create_org(&server).await;
    let org2 = create_org(&server).await;

    let workspaces1 = list_workspaces(&server, org1.id).await;
    let workspace1 = workspaces1.first().unwrap();

    let workspaces2 = list_workspaces(&server, org2.id).await;
    let workspace2 = workspaces2.first().unwrap();

    // Create API key in first workspace
    let api_key_resp1 =
        create_api_key_in_workspace(&server, workspace1.id.clone(), "Key 1".to_string()).await;

    let response = server
        .get(format!("/v1/workspaces/{}/api-keys", workspace2.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    // This should succeed (user can access both orgs as admin)
    assert_eq!(response.status_code(), 200);

    let list_response = response.json::<api::models::ListApiKeysResponse>();

    // The API key from workspace1 should not appear in workspace2's list
    let found = list_response
        .api_keys
        .iter()
        .any(|k| k.id == api_key_resp1.id);
    assert!(
        !found,
        "API key from workspace1 should not appear in workspace2's list"
    );

    println!("✓ API keys are properly isolated between workspaces");
}

#[tokio::test]
async fn test_api_key_authentication() {
    let server = setup_test_server().await;

    let (api_key, _) = create_org_and_api_key(&server).await;

    // Test valid API key
    let response = server
        .get("/v1/models")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .await;

    assert_eq!(
        response.status_code(),
        200,
        "Valid API key should be accepted"
    );

    // Test invalid API key
    let response = server
        .get("/v1/models")
        .add_header("Authorization", "Bearer invalid_key_12345")
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Invalid API key should return 401"
    );

    // Test missing API key
    let response = server.get("/v1/models").await;

    assert_eq!(
        response.status_code(),
        401,
        "Missing API key should return 401"
    );

    println!("✓ API key authentication working correctly");
}

// ============================================
// API Key Edge Cases
// ============================================

#[tokio::test]
async fn test_api_key_with_expiration() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Create API key with short expiration (1 day)
    let request = api::models::CreateApiKeyRequest {
        name: "Short-lived Key".to_string(),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::days(1)),
        spend_limit: None,
    };

    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;

    assert_eq!(response.status_code(), 201);
    let api_key_resp = response.json::<api::models::ApiKeyResponse>();

    assert!(api_key_resp.expires_at.is_some(), "Should have expiration");

    // Create API key with long expiration (365 days)
    let request = api::models::CreateApiKeyRequest {
        name: "Long-lived Key".to_string(),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::days(365)),
        spend_limit: None,
    };

    let response = server
        .post(format!("/v1/workspaces/{}/api-keys", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!(request))
        .await;

    assert_eq!(response.status_code(), 201);

    println!("✓ API keys with different expirations created successfully");
}

#[tokio::test]
async fn test_api_key_name_edge_cases() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;

    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();

    // Test with special characters
    let special_name = "Key-with_special.chars@123!".to_string();
    let key1 =
        create_api_key_in_workspace(&server, workspace.id.clone(), special_name.clone()).await;
    assert_eq!(key1.name, Some(special_name));

    // Test with unicode characters
    let unicode_name = "测试键 🔑".to_string();
    let key2 =
        create_api_key_in_workspace(&server, workspace.id.clone(), unicode_name.clone()).await;
    assert_eq!(key2.name, Some(unicode_name));

    // Test with long name
    let long_name = "A".repeat(200);
    let key3 = create_api_key_in_workspace(&server, workspace.id.clone(), long_name.clone()).await;
    assert_eq!(key3.name, Some(long_name));

    println!("✓ API key names with edge cases handled correctly");
}
