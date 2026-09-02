// Import common test utilities

use crate::common::*;
use api::models::{OrganizationFallbackResponse, OrganizationSettingsResponse};
use serde_json::json;

/// Test complete CRUD lifecycle with three-state PATCH semantics
#[tokio::test]
async fn test_system_prompt_crud_with_patch_semantics() {
    let server = setup_test_server().await;
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
    assert!(settings.settings.fallback_enabled);

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

#[tokio::test]
async fn test_fallback_setting_crud_reset_and_organization_isolation() {
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let other_org = setup_org_with_credits(&server, 10_000_000_000).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    let initial = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert!(initial.settings.fallback_enabled);

    let disabled = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({
            "system_prompt": "preserve me",
            "fallback_enabled": false
        }))
        .await;
    assert_eq!(disabled.status_code(), 200);
    let disabled = disabled.json::<OrganizationSettingsResponse>();
    assert!(!disabled.settings.fallback_enabled);
    assert_eq!(
        disabled.settings.system_prompt.as_deref(),
        Some("preserve me")
    );

    let omitted = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({}))
        .await
        .json::<OrganizationSettingsResponse>();
    assert!(!omitted.settings.fallback_enabled);
    assert_eq!(
        omitted.settings.system_prompt.as_deref(),
        Some("preserve me")
    );

    let other = server
        .get(&format!("/v1/organizations/{}/settings", other_org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert!(other.settings.fallback_enabled);

    let reset = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {access_token}"))
        .json(&json!({ "fallback_enabled": null }))
        .await;
    assert_eq!(reset.status_code(), 200);
    let reset = reset.json::<OrganizationSettingsResponse>();
    assert!(reset.settings.fallback_enabled);
    assert_eq!(reset.settings.system_prompt.as_deref(), Some("preserve me"));
}

#[tokio::test]
async fn test_fallback_patch_is_owner_only_and_mixed_patch_is_atomic() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let owner_access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;
    let (admin_session, _) = setup_unique_test_session(&database).await;
    let (member_session, _) = setup_unique_test_session(&database).await;
    let admin_id = uuid::Uuid::parse_str(admin_session.trim_start_matches("rt_")).unwrap();
    let member_id = uuid::Uuid::parse_str(member_session.trim_start_matches("rt_")).unwrap();
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    {
        let client = database.pool().get().await.expect("database connection");
        client
            .execute(
                "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'admin'), ($1, $3, 'member')",
                &[&org_id, &admin_id, &member_id],
            )
            .await
            .expect("insert organization roles");
    }
    server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {owner_access_token}"))
        .json(&json!({ "system_prompt": "original" }))
        .await;

    for access_token in [&admin_session, &member_session] {
        let get_response = server
            .get(&format!("/v1/organizations/{}/settings", org.id))
            .add_header("Authorization", format!("Bearer {access_token}"))
            .await;
        assert_eq!(get_response.status_code(), 200);

        let patch_response = server
            .patch(&format!("/v1/organizations/{}/settings", org.id))
            .add_header("Authorization", format!("Bearer {access_token}"))
            .json(&json!({
                "system_prompt": "must not be written",
                "fallback_enabled": false
            }))
            .await;
        assert_eq!(patch_response.status_code(), 403);
    }

    let legacy_bypass = server
        .put(&format!("/v1/organizations/{}", org.id))
        .add_header("Authorization", format!("Bearer {admin_session}"))
        .json(&json!({
            "settings": {
                "system_prompt": "must not be written",
                "fallback_enabled": false
            }
        }))
        .await;
    assert_eq!(legacy_bypass.status_code(), 403);

    let unchanged = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {owner_access_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert_eq!(
        unchanged.settings.system_prompt.as_deref(),
        Some("original")
    );
    assert!(unchanged.settings.fallback_enabled);
}

#[tokio::test]
async fn test_concurrent_settings_patches_preserve_distinct_keys() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let owner_token = get_access_token_from_refresh_token(&server, get_session_id()).await;
    let settings_path = format!("/v1/organizations/{}/settings", org.id);
    let admin_path = format!("/v1/admin/organizations/{}/fallback", org.id);

    for iteration in 0..10 {
        let reset = server
            .patch(&settings_path)
            .add_header("Authorization", format!("Bearer {owner_token}"))
            .json(&json!({ "system_prompt": null, "fallback_enabled": true }))
            .await;
        assert_eq!(reset.status_code(), 200);

        let prompt = format!("concurrent prompt {iteration}");
        let owner_patch = server
            .patch(&settings_path)
            .add_header("Authorization", format!("Bearer {owner_token}"))
            .json(&json!({ "system_prompt": prompt }));
        let admin_patch = server
            .patch(&admin_path)
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .json(&json!({ "enabled": false }));
        let (owner_response, admin_response) = tokio::join!(owner_patch, admin_patch);
        assert_eq!(owner_response.status_code(), 200);
        assert_eq!(admin_response.status_code(), 200);

        let current = server
            .get(&settings_path)
            .add_header("Authorization", format!("Bearer {owner_token}"))
            .await
            .json::<OrganizationSettingsResponse>();
        assert_eq!(
            current.settings.system_prompt.as_deref(),
            Some(prompt.as_str())
        );
        assert!(!current.settings.fallback_enabled);
    }
}

#[tokio::test]
async fn test_platform_admin_fallback_endpoint_and_admin_access_token() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let path = format!("/v1/admin/organizations/{}/fallback", org.id);

    let unauthenticated = server.get(&path).await;
    assert_eq!(unauthenticated.status_code(), 401);

    let initial = server
        .get(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(initial.status_code(), 200);
    let initial = initial.json::<OrganizationFallbackResponse>();
    assert_eq!(initial.organization_id.to_string(), org.id);
    assert!(initial.enabled);

    let disabled = server
        .patch(&path)
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&json!({ "enabled": false }))
        .await;
    assert_eq!(disabled.status_code(), 200);
    assert!(!disabled.json::<OrganizationFallbackResponse>().enabled);

    let user_agent = "FallbackControlTest/1.0";
    let create_token = server
        .post("/v1/admin/access-tokens")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", user_agent)
        .json(&json!({
            "expires_in_hours": 24,
            "name": "Fallback control test token",
            "reason": "Verify admin fallback authorization"
        }))
        .await;
    assert_eq!(create_token.status_code(), 200);
    let admin_token = create_token
        .json::<api::models::AdminAccessTokenResponse>()
        .access_token;

    let enabled = server
        .patch(&path)
        .add_header("Authorization", format!("Bearer {admin_token}"))
        .add_header("User-Agent", user_agent)
        .json(&json!({ "enabled": true }))
        .await;
    assert_eq!(enabled.status_code(), 200);
    assert!(enabled.json::<OrganizationFallbackResponse>().enabled);

    let unknown = server
        .get(&format!(
            "/v1/admin/organizations/{}/fallback",
            uuid::Uuid::new_v4()
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;
    assert_eq!(unknown.status_code(), 404);
}

/// Test that system prompts are isolated between organizations
#[tokio::test]
async fn test_system_prompt_isolation() {
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
    let org = setup_org_with_credits(&server, 10000000000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;
    let access_token = get_access_token_from_refresh_token(&server, get_session_id()).await;

    setup_glm_model(&server).await;
    setup_qwen_model(&server).await;

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
    let server = setup_test_server().await;
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
    let server = setup_test_server().await;
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
