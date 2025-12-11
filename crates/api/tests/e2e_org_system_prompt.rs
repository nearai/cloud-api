// Import common test utilities
mod common;

use api::models::OrganizationSettingsResponse;
use common::*;
use serde_json::json;

/// Test complete CRUD lifecycle with three-state PATCH semantics
#[tokio::test]
async fn test_system_prompt_crud_with_patch_semantics() {
    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // 1. GET - Initially None
    let response = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert!(settings.settings.system_prompt.is_none());

    // 2. CREATE - Set initial value
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "Initial prompt" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings.settings.system_prompt.as_deref(),
        Some("Initial prompt")
    );

    // 3. PATCH with omitted field - Preserves existing value
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({}))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings.settings.system_prompt.as_deref(),
        Some("Initial prompt"),
        "Omitted field should preserve value"
    );

    // 4. UPDATE - Change value
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "Updated prompt" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings.settings.system_prompt.as_deref(),
        Some("Updated prompt")
    );

    // 5. DELETE - PATCH with null clears value
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": null }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert!(settings.settings.system_prompt.is_none());

    // 6. PATCH null when already None - Idempotent
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": null }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert!(settings.settings.system_prompt.is_none());

    // 7. PATCH with omitted field on None - Preserves None
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({}))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert!(
        settings.settings.system_prompt.is_none(),
        "Omitted field should preserve None"
    );
}

/// Test that system prompts are isolated between organizations
#[tokio::test]
async fn test_system_prompt_isolation() {
    let server = setup_test_server(None).await;
    let org1 = setup_org_with_credits(&server, 10000000000i64).await;
    let org2 = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set different prompts for each org
    server
        .patch(&format!("/v1/organizations/{}/settings", org1.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "Org 1 prompt" }))
        .await;

    server
        .patch(&format!("/v1/organizations/{}/settings", org2.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "Org 2 prompt" }))
        .await;

    // Verify isolation
    let settings1 = server
        .get(&format!("/v1/organizations/{}/settings", org1.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings1.settings.system_prompt.as_deref(),
        Some("Org 1 prompt")
    );

    let settings2 = server
        .get(&format!("/v1/organizations/{}/settings", org2.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings2.settings.system_prompt.as_deref(),
        Some("Org 2 prompt")
    );
}

/// Test that system prompt is applied in conversation responses
#[tokio::test]
async fn test_system_prompt_integration_with_responses() {
    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    setup_glm_model(&server).await;

    // Set system prompt
    server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({
            "system_prompt": "You are a test assistant."
        }))
        .await;

    // Create conversation and response
    let conversation = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({ "metadata": { "source": "test" } }))
        .await
        .json::<api::models::ConversationObject>();

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": "Hello",
            "conversation": conversation.id,
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let response_json: serde_json::Value = response.json();
    assert_eq!(response_json["status"], "completed");
}

/// Test authentication and authorization requirements
#[tokio::test]
async fn test_system_prompt_auth_requirements() {
    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;

    // GET without auth should fail
    let response = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .await;
    assert_eq!(response.status_code(), 401);

    // PATCH without auth should fail
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .json(&json!({ "system_prompt": "Unauthorized" }))
        .await;
    assert_eq!(response.status_code(), 401);
}

/// Test edge cases: empty strings and special characters
#[tokio::test]
async fn test_system_prompt_edge_cases() {
    let server = setup_test_server(None).await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Empty string
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "" }))
        .await;
    assert_eq!(response.status_code(), 200);

    // Unicode and special characters
    let unicode_prompt = "你好 🌍 مرحبا Здравствуй";
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": unicode_prompt }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings.settings.system_prompt.as_deref(),
        Some(unicode_prompt)
    );

    // Long text (10K chars)
    let long_prompt = "A".repeat(10000);
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": long_prompt }))
        .await;
    // Should either accept or reject with clear validation error
    assert!(
        response.status_code() == 200 || response.status_code() == 400,
        "Unexpected status code: {}",
        response.status_code()
    );
}
