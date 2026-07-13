use super::{bearer, create_reporting_token, setup_reporting_usage_server};
use crate::common::{create_org, get_session_id, MOCK_USER_AGENT, MOCK_USER_ID};
use chrono::{Duration, Utc};
use serde_json::Value;
use services::reporting_tokens::ports::{
    CreateOrganizationReportingTokenRequest, OrganizationReportingTokenRepository as _,
};
use uuid::Uuid;

#[tokio::test]
async fn reporting_token_auth_allows_reporting_only() {
    // Given: a reporting token issued for an organization.
    let (server, _database) = setup_reporting_usage_server().await;
    let org = create_org(&server).await;
    let raw_token = create_reporting_token(&server, &org.id).await;

    // When: the token calls the reporting-authenticated probe route.
    let response = server
        .get(
            format!(
                "/v1/organizations/{}/usage/reporting-token-auth-probe",
                org.id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&raw_token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: the probe sees the read-only reporting token context.
    assert_eq!(response.status_code(), 200, "{}", response.text());
    let body = response.json::<Value>();
    assert_eq!(body["organization_id"], org.id);
    assert_eq!(body["token_prefix"], raw_token[..12]);
    assert_eq!(body["scope"], "usage:read");
    assert!(
        body.get("token").is_none(),
        "probe response must not expose raw reporting token"
    );
    let listed = server
        .get(format!("/v1/organizations/{}/reporting-tokens", org.id).as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
        .json::<Value>();
    let used_token = listed["reporting_tokens"]
        .as_array()
        .expect("reporting_tokens should be an array")
        .iter()
        .find(|token| token["token_prefix"] == raw_token[..12])
        .expect("validated token should still be listed");
    assert!(
        used_token["last_used_at"].as_str().is_some(),
        "successful reporting-token auth should update last_used_at"
    );
    println!("manual GET /usage/reporting-token-auth-probe 200 {body}");
}

#[tokio::test]
async fn reporting_token_auth_rejects_invalid_revoked_expired_and_org_mismatch() {
    // Given: active, revoked, and expired reporting tokens across two organizations.
    let (server, database) = setup_reporting_usage_server().await;
    let org = create_org(&server).await;
    let other_org = create_org(&server).await;
    let active_token = create_reporting_token(&server, &org.id).await;
    let revoked_token = create_reporting_token(&server, &org.id).await;
    let expired = database
        .organization_reporting_tokens
        .create(CreateOrganizationReportingTokenRequest {
            organization_id: Uuid::parse_str(&org.id).expect("org id should be uuid"),
            name: "expired reporting token".to_string(),
            created_by_user_id: Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be uuid"),
            expires_at: Some(Utc::now() - Duration::minutes(1)),
        })
        .await
        .expect("expired reporting token fixture should be created");
    let revoked_list = server
        .get(format!("/v1/organizations/{}/reporting-tokens", org.id).as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await
        .json::<Value>();
    let revoked_id = revoked_list["reporting_tokens"]
        .as_array()
        .expect("reporting_tokens should be an array")
        .iter()
        .find(|token| token["token_prefix"] == revoked_token[..12])
        .and_then(|token| token["id"].as_str())
        .expect("revoked token id should be listed before revoke")
        .to_string();
    let revoke_response = server
        .delete(format!("/v1/organizations/{}/reporting-tokens/{revoked_id}", org.id).as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        revoke_response.status_code(),
        204,
        "{}",
        revoke_response.text()
    );

    let probe_url = format!(
        "/v1/organizations/{}/usage/reporting-token-auth-probe",
        org.id
    );

    // When/Then: missing, non-Bearer, malformed, revoked, and expired credentials are rejected.
    let missing = server.get(&probe_url).await;
    assert_eq!(missing.status_code(), 401, "{}", missing.text());

    let non_bearer = server
        .get(&probe_url)
        .add_header("Authorization", "Basic rpt-not-a-bearer")
        .await;
    assert_eq!(non_bearer.status_code(), 401, "{}", non_bearer.text());

    let malformed = server
        .get(&probe_url)
        .add_header("Authorization", "Bearer rpt-short")
        .await;
    assert_eq!(malformed.status_code(), 401, "{}", malformed.text());

    let revoked = server
        .get(&probe_url)
        .add_header("Authorization", bearer(&revoked_token))
        .await;
    assert_eq!(revoked.status_code(), 401, "{}", revoked.text());

    let expired_response = server
        .get(&probe_url)
        .add_header("Authorization", bearer(&expired.raw_token))
        .await;
    assert_eq!(
        expired_response.status_code(),
        401,
        "{}",
        expired_response.text()
    );

    // When: a valid reporting token is used against a different organization's route.
    let mismatch = server
        .get(
            format!(
                "/v1/organizations/{}/usage/reporting-token-auth-probe",
                other_org.id
            )
            .as_str(),
        )
        .add_header("Authorization", bearer(&active_token))
        .await;

    // Then: the route rejects the org mismatch after authentication.
    assert_eq!(mismatch.status_code(), 403, "{}", mismatch.text());
}

#[tokio::test]
async fn reporting_token_cannot_infer_or_mutate() {
    // Given: a reporting token issued for an organization.
    let (server, _database) = setup_reporting_usage_server().await;
    let org = create_org(&server).await;
    let raw_token = create_reporting_token(&server, &org.id).await;

    // When: the reporting token attempts to call data-plane and management routes.
    let inference = server
        .post("/v1/chat/completions")
        .add_header("Authorization", bearer(&raw_token))
        .json(&serde_json::json!({
            "model": "Qwen/Qwen3-30B-A3B-Instruct-2507",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .await;
    let workspace_mutation = server
        .post(format!("/v1/organizations/{}/workspaces", org.id).as_str())
        .add_header("Authorization", bearer(&raw_token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "name": "must not create" }))
        .await;
    let token_management = server
        .post(format!("/v1/organizations/{}/reporting-tokens", org.id).as_str())
        .add_header("Authorization", bearer(&raw_token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "name": "must not create" }))
        .await;
    let session_management = server
        .delete("/v1/users/me/tokens")
        .add_header("Authorization", bearer(&raw_token))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: reporting tokens do not authorize inference, workspace, token management,
    // or session management routes.
    assert_eq!(inference.status_code(), 401, "{}", inference.text());
    assert_eq!(
        workspace_mutation.status_code(),
        401,
        "{}",
        workspace_mutation.text()
    );
    assert_eq!(
        token_management.status_code(),
        401,
        "{}",
        token_management.text()
    );
    assert_eq!(
        session_management.status_code(),
        401,
        "{}",
        session_management.text()
    );
}
