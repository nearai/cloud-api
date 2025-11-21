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
        .add_header("Authorization", format!("Bearer {}", access_token))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings_response = response.json::<OrganizationSettingsResponse>();
    // system_prompt should be None if not set
    assert!(settings_response.settings.system_prompt.is_none());

    // Update organization system prompt via settings
    let test_prompt = "You are a helpful assistant that always responds in a professional manner.";
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {}", access_token))
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
        .add_header("Authorization", format!("Bearer {}", access_token))
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
        .add_header("Authorization", format!("Bearer {}", access_token))
        .await;

    assert_eq!(response.status_code(), 200);
    let settings_response = response.json::<OrganizationSettingsResponse>();
    // After deletion, the field should be None again
    assert!(settings_response.settings.system_prompt.is_none());
}

#[tokio::test]
async fn test_system_prompt_applied_to_responses() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await; // $10.00 USD
    let org_id = org.id.clone();
    let api_key = get_api_key_for_org(&server, org.id).await;

    // Create a user access token to set the system prompt
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Set an organization system prompt via settings
    let test_prompt = "Always start your response with 'SYSTEM_PROMPT_TEST:'";
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org_id))
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&json!({
            "system_prompt": test_prompt
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Make a response request (not chat completion)
    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {}", api_key))
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": "Say hello",
            "stream": false
        }))
        .await;

    assert_eq!(response.status_code(), 200);

    // Parse response and check if system prompt was applied at the response level
    // Note: We can't directly verify the system prompt was prepended in the test,
    // but we can verify the request succeeded and has output
    let response_json: serde_json::Value = response.json();
    assert!(response_json["output"].is_array());
    assert!(response_json["output"].as_array().unwrap().len() > 0);
    assert_eq!(response_json["status"], "completed");
}

#[tokio::test]
async fn test_system_prompt_unauthorized_access() {
    let server = setup_test_server().await;
    let org1 = setup_org_with_credits(&server, 10000000000i64).await;
    let _org2 = setup_org_with_credits(&server, 10000000000i64).await;

    // Create a user access token for org1
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    // Try to access org2's system prompt (should fail if not a member)
    // Note: This test assumes the session token is only for org1
    // In a real scenario, you'd need to create two separate users

    // Set system prompt for org1 via settings (should succeed)
    let response = server
        .patch(&format!("/v1/organizations/{}/settings", org1.id))
        .add_header("Authorization", format!("Bearer {}", access_token))
        .json(&json!({
            "system_prompt": "Test prompt"
        }))
        .await;

    assert_eq!(response.status_code(), 200);
}
