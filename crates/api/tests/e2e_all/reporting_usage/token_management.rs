use super::{
    assert_json_has_no_secret_fields, bearer, redact_create_response, setup_reporting_usage_server,
};
use crate::common::{create_org, get_session_id, setup_unique_test_session, MOCK_USER_AGENT};
use chrono::{Duration, Utc};
use serde_json::Value;

#[tokio::test]
async fn reporting_token_management() {
    // Given: an organization owned by the authenticated session user.
    let (server, _database) = setup_reporting_usage_server().await;
    let org = create_org(&server).await;
    let url = format!("/v1/organizations/{}/reporting-tokens", org.id);

    // When: the owner creates a reporting token.
    let create_response = server
        .post(&url)
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "name": "finance export",
            "expires_at": (Utc::now() + Duration::days(30)).to_rfc3339(),
        }))
        .await;

    // Then: the create response exposes the raw token exactly once and no hash.
    assert_eq!(
        create_response.status_code(),
        201,
        "{}",
        create_response.text()
    );
    let created = create_response.json::<Value>();
    let raw_token = created
        .get("token")
        .and_then(Value::as_str)
        .expect("create response should include raw reporting token once");
    assert!(
        raw_token.starts_with("rpt-"),
        "raw reporting token should use rpt- prefix"
    );
    assert_eq!(
        created.to_string().matches(raw_token).count(),
        1,
        "raw reporting token should appear exactly once in create response"
    );
    assert!(
        !created.to_string().contains("token_hash"),
        "create response must not expose token_hash"
    );
    println!(
        "manual POST /reporting-tokens 201 {}",
        redact_create_response(&created)
    );
    let token_id = created
        .get("id")
        .and_then(Value::as_str)
        .expect("create response should include token id")
        .to_string();

    // When: the owner lists reporting tokens.
    let list_response = server
        .get(&url)
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: list includes metadata only and never includes raw token material.
    assert_eq!(list_response.status_code(), 200, "{}", list_response.text());
    let listed = list_response.json::<Value>();
    assert_json_has_no_secret_fields(&listed);
    println!("manual GET /reporting-tokens 200 {listed}");
    let tokens = listed
        .get("reporting_tokens")
        .and_then(Value::as_array)
        .expect("list response should include reporting_tokens array");
    assert_eq!(
        tokens.len(),
        1,
        "created token should be listed while active"
    );
    assert_eq!(tokens[0]["id"], token_id);
    assert_eq!(tokens[0]["name"], "finance export");

    // When: the owner revokes the reporting token.
    let revoke_response = server
        .delete(format!("{url}/{token_id}").as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    // Then: the token is revoked and absent from active listings.
    assert_eq!(
        revoke_response.status_code(),
        204,
        "{}",
        revoke_response.text()
    );
    println!("manual DELETE /reporting-tokens/{{token_id}} 204");
    let after_revoke_response = server
        .get(&url)
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        after_revoke_response.status_code(),
        200,
        "{}",
        after_revoke_response.text()
    );
    let after_revoke = after_revoke_response.json::<Value>();
    assert_json_has_no_secret_fields(&after_revoke);
    println!("manual GET /reporting-tokens after revoke 200 {after_revoke}");
    assert_eq!(
        after_revoke["reporting_tokens"]
            .as_array()
            .expect("reporting_tokens should be an array")
            .len(),
        0,
        "revoked token should not appear in active token list"
    );
}

#[tokio::test]
async fn reporting_token_management_forbidden_for_non_member() {
    // Given: an organization owned by the default test session and a second user
    // who is not a member of that organization.
    let (server, database) = setup_reporting_usage_server().await;
    let org = create_org(&server).await;
    let (other_session, _email) = setup_unique_test_session(&database).await;

    // When: the non-member attempts to create a reporting token.
    let response = server
        .post(format!("/v1/organizations/{}/reporting-tokens", org.id).as_str())
        .add_header("Authorization", bearer(&other_session))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "name": "unauthorized export" }))
        .await;

    // Then: the management route denies cross-org access.
    assert_eq!(response.status_code(), 403, "{}", response.text());
}

#[tokio::test]
async fn reporting_token_management_rejects_malformed_input() {
    // Given: an organization owned by the authenticated session user.
    let (server, _database) = setup_reporting_usage_server().await;
    let org = create_org(&server).await;
    let url = format!("/v1/organizations/{}/reporting-tokens", org.id);

    // When/Then: empty names are rejected.
    let empty_name = server
        .post(&url)
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({ "name": "" }))
        .await;
    assert_eq!(empty_name.status_code(), 400, "{}", empty_name.text());

    // When/Then: invalid organization ids are rejected at the HTTP boundary.
    let invalid_org = server
        .get("/v1/organizations/not-a-uuid/reporting-tokens")
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(invalid_org.status_code(), 400, "{}", invalid_org.text());

    // When/Then: invalid token ids are rejected at the HTTP boundary.
    let invalid_token = server
        .delete(format!("{url}/not-a-uuid").as_str())
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(invalid_token.status_code(), 400, "{}", invalid_token.text());

    // When/Then: already-expired reporting tokens are rejected.
    let past_expiry = server
        .post(&url)
        .add_header("Authorization", bearer(&get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&serde_json::json!({
            "name": "already expired",
            "expires_at": (Utc::now() - Duration::minutes(1)).to_rfc3339(),
        }))
        .await;
    assert_eq!(past_expiry.status_code(), 400, "{}", past_expiry.text());
}
