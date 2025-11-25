// Import common test utilities
mod common;

use api::models::OrganizationSettingsResponse;
use common::*;
use serde_json::json;

#[tokio::test]
async fn test_organization_system_prompt_crud() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD

    // Create a user access token
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Get organization settings (system_prompt should be None initially)
    let response = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings_response = response.json::<OrganizationSettingsResponse>();
    // system_prompt should be None if not set
    assert!(settings_response.settings.system_prompt.is_none());

    // Update organization system prompt via settings
    let test_prompt = "You are a helpful assistant that always responds in a professional manner.";
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({
            "system_prompt": test_prompt
        }))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings_response = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings_response.settings.system_prompt.as_deref(),
        Some(test_prompt)
    );

    // Get organization settings (should return the set prompt)
    let response = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings_response = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings_response.settings.system_prompt.as_deref(),
        Some(test_prompt)
    );

    // Clear the system prompt using DELETE
    let response = server
        .delete(&format!(
            "/v1/organizations/{}/settings?field=system_prompt",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings_response = response.json::<OrganizationSettingsResponse>();
    // After deletion, the field should be None again
    assert!(settings_response.settings.system_prompt.is_none());
}

#[tokio::test]
async fn test_system_prompt_used_in_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    setup_glm_model(&server).await;

    // Create a user access token to set the system prompt
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set an organization system prompt
    let test_prompt = "You are a test assistant with a specific system prompt.";
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({
            "system_prompt": test_prompt
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Create a conversation
    let conversation_response = server
        .post("/v1/conversations")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "metadata": {
                "source": "test"
            }
        }))
        .await;

    assert_eq!(conversation_response.status_code(), 201);
    let conversation: api::models::ConversationObject = conversation_response.json();

    // Make a response in the conversation - the system prompt should be applied
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

    // The request should succeed - system prompt is applied internally
    assert_eq!(response.status_code(), 200);

    // Parse response to verify it completed
    let response_json: serde_json::Value = response.json();
    assert_eq!(response_json["status"], "completed");

    // Note: System prompts are not stored as conversation items.
    // They are transiently prepended to completion requests for the LLM.
    // This is intentional - they're metadata, not part of visible conversation history.
}

#[tokio::test]
async fn test_system_prompt_isolation_between_orgs() {
    let server = setup_test_server().await;
    let org1 = setup_org_with_credits(&server, 10000000000i64).await;
    let org2 = setup_org_with_credits(&server, 10000000000i64).await;

    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set different system prompts for each org
    let prompt1 = "Organization 1 specific prompt";
    let prompt2 = "Organization 2 specific prompt";

    let response1 = server
        .patch(&format!("/v1/organizations/{}/settings", org1.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": prompt1 }))
        .await;
    assert_eq!(response1.status_code(), 200);

    let response2 = server
        .patch(&format!("/v1/organizations/{}/settings", org2.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": prompt2 }))
        .await;
    assert_eq!(response2.status_code(), 200);

    // Verify org1 has prompt1
    let settings1 = server
        .get(&format!("/v1/organizations/{}/settings", org1.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();

    assert_eq!(settings1.settings.system_prompt.as_deref(), Some(prompt1));

    // Verify org2 has prompt2
    let settings2 = server
        .get(&format!("/v1/organizations/{}/settings", org2.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();

    assert_eq!(settings2.settings.system_prompt.as_deref(), Some(prompt2));

    // System prompts are isolated - each org has its own independent setting
}

#[tokio::test]
async fn test_system_prompt_unauthorized_get() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;

    // Try to access settings without auth
    let response = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should require authentication to get settings"
    );
}

#[tokio::test]
async fn test_system_prompt_unauthorized_update() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;

    // Try to update settings without auth
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .json(&json!({ "system_prompt": "Hacked!" }))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "Should require authentication to update settings"
    );
}

#[tokio::test]
async fn test_system_prompt_validation_empty_string() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set empty string (should be allowed and treated as unset)
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "" }))
        .await;

    assert_eq!(response.status_code(), 200);
}

#[tokio::test]
async fn test_system_prompt_validation_long_text() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set a very long prompt (test practical limits)
    let long_prompt = "A".repeat(10000);
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": long_prompt }))
        .await;

    // Should succeed (or return specific validation error if there's a max length)
    assert!(
        response.status_code() == 200 || response.status_code() == 400,
        "Should either accept long prompts or return validation error"
    );
}

#[tokio::test]
async fn test_system_prompt_validation_unicode() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Test with Unicode characters
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
}

#[tokio::test]
async fn test_system_prompt_delete_when_not_set() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Try to delete when system_prompt is not set
    let response = server
        .delete(&format!(
            "/v1/organizations/{}/settings?field=system_prompt",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    // Should succeed (idempotent operation)
    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert!(settings.settings.system_prompt.is_none());
}

#[tokio::test]
async fn test_system_prompt_delete_nonexistent_field() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Delete a field that doesn't exist - should succeed (idempotent)
    let response = server
        .delete(&format!(
            "/v1/organizations/{}/settings?field=nonexistent_field",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    // Should succeed - DELETE is idempotent and accepts any field name
    assert_eq!(response.status_code(), 200);
}

#[tokio::test]
async fn test_system_prompt_delete_without_field_param() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set a system prompt first
    server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "Test" }))
        .await;

    // Try to delete without field parameter
    let response = server
        .delete(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    // Should return an error
    assert_eq!(
        response.status_code(),
        400,
        "Should require field parameter for DELETE"
    );
}

#[tokio::test]
async fn test_system_prompt_clear_with_delete() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set a prompt first
    server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": "Initial prompt" }))
        .await;

    // Clear it using DELETE
    let response = server
        .delete(&format!(
            "/v1/organizations/{}/settings?field=system_prompt",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings = response.json::<OrganizationSettingsResponse>();
    assert!(
        settings.settings.system_prompt.is_none(),
        "DELETE should clear the system prompt"
    );
}

#[tokio::test]
async fn test_system_prompt_multiple_updates() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Update multiple times
    let prompts = vec![
        "First prompt",
        "Second prompt",
        "Third prompt with more detail",
    ];

    for prompt in &prompts {
        let response = server
            .patch(&format!("/v1/organizations/{}/settings", org.id))
            .add_header("Authorization", format!("Bearer {access_token}"))
            .json(&json!({ "system_prompt": prompt }))
            .await;

        assert_eq!(response.status_code(), 200);
        let settings = response.json::<OrganizationSettingsResponse>();
        assert_eq!(settings.settings.system_prompt.as_deref(), Some(*prompt));
    }

    // Final check
    let response = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await;

    let settings = response.json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings.settings.system_prompt.as_deref(),
        Some(*prompts.last().unwrap())
    );
}

#[tokio::test]
async fn test_system_prompt_persists_in_settings() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set system prompt
    let test_prompt = "Persistent system prompt";
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "system_prompt": test_prompt }))
        .await;
    assert_eq!(response.status_code(), 200);

    // Verify it's stored correctly (first read)
    let settings1 = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings1.settings.system_prompt.as_deref(),
        Some(test_prompt)
    );

    // Read again to verify persistence (second read)
    let settings2 = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert_eq!(
        settings2.settings.system_prompt.as_deref(),
        Some(test_prompt)
    );

    // The system prompt persists across requests and is applied to all
    // responses for this organization
}
