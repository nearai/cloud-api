// Import common test utilities

use crate::common::*;
use api::models::OrganizationSettingsResponse;
use serde_json::json;
use services::common::RepositoryError;
use services::organization::{OrganizationRepository, UpdateOrganizationRequest};

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
async fn test_legacy_full_settings_update_rejects_stale_fallback_snapshot() {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let owner_token = get_access_token_from_refresh_token(&server, get_session_id()).await;
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let (admin_session, _) = setup_unique_test_session(&database).await;
    let admin_id = uuid::Uuid::parse_str(admin_session.trim_start_matches("rt_")).unwrap();
    database
        .pool()
        .get()
        .await
        .expect("database connection")
        .execute(
            "INSERT INTO organization_members (organization_id, user_id, role) VALUES ($1, $2, 'admin')",
            &[&org_id, &admin_id],
        )
        .await
        .expect("insert organization admin");

    let disabled = server
        .patch(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {owner_token}"))
        .json(&json!({ "fallback_enabled": false }))
        .await;
    assert_eq!(disabled.status_code(), 200);

    // This is the repository operation a legacy admin request would issue
    // after it had read the former `true` override. The atomic guard must stop
    // that stale full-settings object from restoring the owner's old policy.
    let repository = database::repositories::PgOrganizationRepository::new(database.pool().clone());
    let stale = repository
        .update(
            org_id,
            UpdateOrganizationRequest {
                name: None,
                description: None,
                rate_limit: None,
                settings: Some(json!({
                    "fallback_enabled": true,
                    "unrelated": "stale admin update"
                })),
            },
            Some(Some(json!(true))),
            admin_id,
        )
        .await;
    assert!(matches!(stale, Err(RepositoryError::TransactionConflict)));

    let current = server
        .get(&format!("/v1/organizations/{}/settings", org.id))
        .add_header("Authorization", format!("Bearer {owner_token}"))
        .await
        .json::<OrganizationSettingsResponse>();
    assert!(!current.settings.fallback_enabled);
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

/// Test that system prompt is applied to a stateless Responses request.
#[tokio::test]
async fn test_system_prompt_integration_with_stateless_responses() {
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

    let response = server
        .post("/v1/responses")
        .add_header("Authorization", format!("Bearer {api_key}"))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "input": "Hello",
            "store": false,
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
