// E2E tests for attestation route authentication (nearai/infra#193):
//
// Route classification under test (mirrors crates/api/src/routes/attestation.rs):
//   - GET /v1/attestation/report    -> API key required (non-billable)
//   - GET /v1/signature/{chat_id}   -> API key required (unchanged)
//   - GET /v1/attestation/ita-token -> public (explicit, documented decision)
//
// Covers: missing/malformed/invalid/expired/revoked credentials -> 401,
// valid key -> 200 across query variants, and that report retrieval creates
// no usage/billing records.

use crate::common::*;

fn report_url(model: &str) -> String {
    let encoded = url::form_urlencoded::byte_serialize(model.as_bytes()).collect::<String>();
    format!("/v1/attestation/report?model={encoded}")
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /v1/attestation/report — credential rejection matrix
// ─────────────────────────────────────────────────────────────────────────────

/// Missing Authorization header must return the standard 401 envelope.
#[tokio::test]
async fn test_attestation_report_missing_auth_returns_401() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;

    let response = server.get(&report_url(&model)).await;

    assert_eq!(
        response.status_code(),
        401,
        "unauthenticated attestation report must 401, got: {}",
        response.text()
    );
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["type"], "missing_auth_header",
        "unexpected 401 envelope: {body}"
    );
}

/// A malformed Authorization header (non-Bearer scheme) must return 401.
#[tokio::test]
async fn test_attestation_report_malformed_auth_returns_401() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;

    let response = server
        .get(&report_url(&model))
        .add_header("Authorization", "Basic dXNlcjpwYXNz")
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "malformed auth header must 401, got: {}",
        response.text()
    );
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["type"], "invalid_auth_header",
        "unexpected 401 envelope: {body}"
    );
}

/// An invalid (never-issued) API key must return 401.
#[tokio::test]
async fn test_attestation_report_invalid_key_returns_401() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;

    let response = server
        .get(&report_url(&model))
        .add_header("Authorization", "Bearer sk-this-key-does-not-exist")
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "invalid API key must 401, got: {}",
        response.text()
    );
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["type"], "invalid_api_key",
        "unexpected 401 envelope: {body}"
    );
}

/// A revoked (deleted) API key must return 401.
#[tokio::test]
async fn test_attestation_report_revoked_key_returns_401() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(
        &server,
        workspace.id.clone(),
        "attestation-revoked-key".to_string(),
    )
    .await;
    let api_key = api_key_resp.key.clone().unwrap();

    // Revoke the key before it is ever used.
    let delete_response = server
        .delete(&format!(
            "/v1/workspaces/{}/api-keys/{}",
            workspace.id, api_key_resp.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(delete_response.status_code(), 204);

    let response = server
        .get(&report_url(&model))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "revoked API key must 401, got: {}",
        response.text()
    );
}

/// An expired API key must return 401.
#[tokio::test]
async fn test_attestation_report_expired_key_returns_401() {
    let (server, database) = setup_test_server_with_database().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key_resp = create_api_key_in_workspace(
        &server,
        workspace.id.clone(),
        "attestation-expired-key".to_string(),
    )
    .await;
    let api_key = api_key_resp.key.clone().unwrap();
    let api_key_id = uuid::Uuid::parse_str(&api_key_resp.id).unwrap();

    // Expire the key before it is ever used (so no validation cache entry exists).
    let client = database.pool().get().await.unwrap();
    let updated = client
        .execute(
            "UPDATE api_keys SET expires_at = NOW() - INTERVAL '1 hour' WHERE id = $1",
            &[&api_key_id],
        )
        .await
        .unwrap();
    assert_eq!(updated, 1, "expected to expire exactly one API key row");

    let response = server
        .get(&report_url(&model))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "expired API key must 401, got: {}",
        response.text()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /v1/attestation/report — valid key across query variants
// ─────────────────────────────────────────────────────────────────────────────

/// A valid API key retrieves the report across supported query variants:
/// bare model, explicit signing algorithms, and a caller-supplied nonce.
#[tokio::test]
async fn test_attestation_report_valid_key_query_variants() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let base = report_url(&model);
    let nonce = "deadbeef".repeat(8); // 32 bytes = 64 hex chars
    let variants = [
        base.clone(),
        format!("{base}&signing_algo=ecdsa"),
        format!("{base}&signing_algo=ed25519"),
        format!("{base}&signing_algo=ecdsa&nonce={nonce}"),
    ];

    for url in &variants {
        let response = server
            .get(url)
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;

        assert_eq!(
            response.status_code(),
            200,
            "authenticated attestation report must succeed for {url}: {}",
            response.text()
        );
        let body: serde_json::Value = response.json();
        assert!(
            body.get("gateway_attestation").is_some(),
            "report body missing gateway_attestation for {url}: {body}"
        );
    }
}

/// Parameter validation still runs behind auth: an invalid nonce is a 400
/// (not a 401) for an authenticated caller.
#[tokio::test]
async fn test_attestation_report_valid_key_invalid_nonce_is_400() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;

    let response = server
        .get(&format!("{}&nonce=ff", report_url(&model)))
        .add_header("Authorization", format!("Bearer {api_key}"))
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "invalid nonce with valid key must 400, got: {}",
        response.text()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Non-billable: report retrieval must not create usage/billing records
// ─────────────────────────────────────────────────────────────────────────────

/// Authenticated report retrieval is key-validated but non-billable: the org
/// usage history must stay empty after successful report calls.
#[tokio::test]
async fn test_attestation_report_creates_no_usage_records() {
    let server = setup_test_server().await;
    let model = setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id.clone()).await;

    for _ in 0..3 {
        let response = server
            .get(&report_url(&model))
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }

    // Allow any (incorrectly) spawned background usage recording to land.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let history_resp = server
        .get(&format!(
            "/v1/organizations/{}/usage/history?limit=10&offset=0",
            org.id
        ))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(
        history_resp.status_code(),
        200,
        "usage history should succeed: {}",
        history_resp.text()
    );
    let history: api::routes::usage::UsageHistoryResponse = history_resp.json();
    assert!(
        history.data.is_empty(),
        "attestation report retrieval must not create usage records, found: {:?}",
        history.data
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Sibling attestation routes — explicit classification tests
// ─────────────────────────────────────────────────────────────────────────────

/// GET /v1/signature/{chat_id} is API-key-protected: unauthenticated lookups
/// must 401 before any signature lookup happens.
#[tokio::test]
async fn test_signature_route_missing_auth_returns_401() {
    let server = setup_test_server().await;

    let response = server
        .get("/v1/signature/chatcmpl-does-not-exist?signing_algo=ecdsa")
        .await;

    assert_eq!(
        response.status_code(),
        401,
        "unauthenticated signature lookup must 401, got: {}",
        response.text()
    );
    let body: serde_json::Value = response.json();
    assert_eq!(
        body["error"]["type"], "missing_auth_header",
        "unexpected 401 envelope: {body}"
    );
}

/// GET /v1/attestation/ita-token stays public — an explicit, documented
/// decision (see build_public_attestation_routes). Without credentials the
/// route must never reject with 401/403; on a server without ITA configured
/// it degrades to 503 instead. The positive public-access path (200 with a
/// fake ITA upstream) is covered by
/// ita_token_is_public_and_returns_gateway_token_when_fake_ita_succeeds.
#[tokio::test]
async fn test_ita_token_route_is_public_no_auth_rejection() {
    let server = setup_test_server().await;

    let response = server.get("/v1/attestation/ita-token").await;

    assert_ne!(
        response.status_code(),
        401,
        "public ita-token route must not require auth: {}",
        response.text()
    );
    assert_ne!(
        response.status_code(),
        403,
        "public ita-token route must not require auth: {}",
        response.text()
    );
}
