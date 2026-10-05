use crate::common::*;
use chrono::{Duration, Utc};
use serde_json::Value;

const FORBIDDEN_KEY_FIELDS: [&str; 5] = ["key", "key_hash", "key_prefix", "keyPrefix", "name"];

fn managed_key_name(generation: u32) -> String {
    format!("Playground-{}-g{generation}", uuid::Uuid::new_v4())
}

async fn list_admin_api_keys(
    server: &axum_test::TestServer,
    query: &str,
) -> axum_test::TestResponse {
    server
        .get(format!("/v1/admin/api-keys?{query}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
}

#[tokio::test]
async fn test_admin_list_api_keys_fields_and_playground_flag() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id.clone())
        .await
        .into_iter()
        .next()
        .expect("organization should have a default workspace");

    let uuid = uuid::Uuid::new_v4();
    let cases: Vec<(String, bool)> = vec![
        (managed_key_name(1), true),
        (managed_key_name(0), true),
        ("prod".to_string(), false),
        ("Playground key".to_string(), false),
        (format!("playground-{uuid}-g1"), false),
        (format!("Playground-{uuid}-gx"), false),
        (format!("Playground-{uuid}-g1-copy"), false),
        (
            format!("Playground-{}-g1", uuid.to_string().to_uppercase()),
            false,
        ),
    ];

    let mut expected = std::collections::HashMap::new();
    for (name, is_managed) in &cases {
        let key = create_api_key_in_workspace(&server, workspace.id.clone(), name.clone()).await;
        expected.insert(key.id.clone(), *is_managed);
    }

    // Revoke the first managed key: revoked keys still count as activations.
    let revoked_id =
        create_api_key_in_workspace(&server, workspace.id.clone(), managed_key_name(2))
            .await
            .id;
    expected.insert(revoked_id.clone(), true);
    let revoke = server
        .delete(format!("/v1/workspaces/{}/api-keys/{revoked_id}", workspace.id).as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(revoke.status_code(), 204);

    let response =
        list_admin_api_keys(&server, &format!("organization_id={}&limit=100", org.id)).await;
    assert_eq!(response.status_code(), 200);
    let raw = response.json::<Value>();
    for key in raw["api_keys"].as_array().expect("api_keys array") {
        for field in FORBIDDEN_KEY_FIELDS {
            assert!(
                key.get(field).is_none(),
                "response must not expose `{field}`"
            );
        }
    }

    let body = response.json::<api::models::ListAdminApiKeysResponse>();
    assert_eq!(body.total, expected.len() as i64);
    assert_eq!(body.api_keys.len(), expected.len());
    assert_eq!(body.limit, 100);
    assert_eq!(body.offset, 0);

    for key in &body.api_keys {
        let is_managed = expected
            .get(&key.id)
            .unwrap_or_else(|| panic!("unexpected key {} in org-scoped listing", key.id));
        assert_eq!(key.is_managed_playground, *is_managed, "key {}", key.id);
        assert_eq!(key.organization_id, org.id);
        assert_eq!(key.organization_name, org.name);
        assert_eq!(key.workspace_id, workspace.id);
        assert_eq!(key.created_by_user_id, MOCK_USER_ID);
        if key.id == revoked_id {
            assert!(
                key.deleted_at.is_some(),
                "revoked key should carry deleted_at"
            );
        } else {
            assert!(key.deleted_at.is_none());
            assert!(key.is_active);
        }
    }

    let created: Vec<_> = body.api_keys.iter().map(|k| k.created_at).collect();
    assert!(
        created.windows(2).all(|w| w[0] >= w[1]),
        "keys should be ordered newest first"
    );
}

#[tokio::test]
async fn test_admin_list_api_keys_filters_scope_results() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let other_org = create_org(&server).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    let _ = get_api_key_for_org(&server, other_org.id.clone()).await;
    assert!(!key.is_empty());

    let response = list_admin_api_keys(&server, &format!("organization_id={}", org.id)).await;
    assert_eq!(response.status_code(), 200);
    let body = response.json::<api::models::ListAdminApiKeysResponse>();
    assert_eq!(body.total, 1);
    assert!(body.api_keys.iter().all(|k| k.organization_id == org.id));

    let future =
        (Utc::now() + Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let response = list_admin_api_keys(
        &server,
        &format!("organization_id={}&created_after={future}", org.id),
    )
    .await;
    assert_eq!(response.status_code(), 200);
    let body = response.json::<api::models::ListAdminApiKeysResponse>();
    assert_eq!(body.total, 0);
    assert!(body.api_keys.is_empty());

    let past = (Utc::now() - Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let response = list_admin_api_keys(
        &server,
        &format!(
            "organization_id={}&created_after={past}&created_before={future}",
            org.id
        ),
    )
    .await;
    assert_eq!(response.status_code(), 200);
    assert_eq!(
        response
            .json::<api::models::ListAdminApiKeysResponse>()
            .total,
        1
    );
}

#[tokio::test]
async fn test_admin_list_api_keys_includes_keys_on_date_bounds() {
    let server = setup_test_server().await;
    let org = create_org(&server).await;
    let _ = get_api_key_for_org(&server, org.id.clone()).await;

    let response = list_admin_api_keys(&server, &format!("organization_id={}", org.id)).await;
    assert_eq!(response.status_code(), 200);
    let created_at = response
        .json::<api::models::ListAdminApiKeysResponse>()
        .api_keys[0]
        .created_at
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);

    // Both bounds equal to the key's exact created_at: inclusive bounds keep it.
    let response = list_admin_api_keys(
        &server,
        &format!(
            "organization_id={}&created_after={created_at}&created_before={created_at}",
            org.id
        ),
    )
    .await;
    assert_eq!(response.status_code(), 200);
    let body = response.json::<api::models::ListAdminApiKeysResponse>();
    assert_eq!(body.total, 1);
    assert_eq!(body.api_keys.len(), 1);
}

#[tokio::test]
async fn test_admin_list_api_keys_rejects_invalid_params() {
    let server = setup_test_server().await;

    let now = Utc::now();
    let after = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let before = (now - Duration::days(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let response = list_admin_api_keys(
        &server,
        &format!("created_after={after}&created_before={before}"),
    )
    .await;
    assert_eq!(response.status_code(), 400);

    let response = list_admin_api_keys(&server, "limit=0").await;
    assert_eq!(response.status_code(), 400);
}

#[tokio::test]
async fn test_admin_list_api_keys_requires_admin_auth() {
    let server = setup_test_server().await;
    let response = server.get("/v1/admin/api-keys").await;
    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn test_admin_list_api_keys_rejects_non_admin_users() {
    let server = setup_test_server_with_config(|config| {
        config.auth.admin_domains = vec!["example.org".to_string()];
    })
    .await;
    let response = list_admin_api_keys(&server, "limit=1").await;
    assert_eq!(response.status_code(), 403);
}
