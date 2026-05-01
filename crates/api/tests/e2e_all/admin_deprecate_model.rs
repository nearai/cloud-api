// E2E tests for POST /v1/admin/models/deprecate

use crate::common::*;
use api::models::{
    AdminModelListResponse, BatchUpdateModelApiRequest, DeprecateModelResponse, ErrorResponse,
};

/// Build a minimal model upsert payload — enough to satisfy the "new model"
/// required-field validation (modelDisplayName, modelDescription, contextLength).
fn minimal_model_upsert(extra_aliases: &[&str]) -> serde_json::Value {
    let mut v = serde_json::json!({
        "inputCostPerToken":  { "amount": 1_000_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
        "modelDisplayName":   "Deprecation Test Model",
        "modelDescription":   "Synthetic model for deprecation e2e",
        "contextLength":      4096,
        "verifiable":         false,
        "isActive":           true,
    });
    if !extra_aliases.is_empty() {
        v["aliases"] = serde_json::json!(extra_aliases);
    }
    v
}

async fn create_model(
    server: &axum_test::TestServer,
    name: &str,
    aliases: &[&str],
) -> api::models::ModelWithPricing {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        name.to_string(),
        serde_json::from_value(minimal_model_upsert(aliases)).unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    updated.into_iter().next().expect("Model should be created")
}

async fn deprecate(
    server: &axum_test::TestServer,
    body: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post("/v1/admin/models/deprecate")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

// ---------------------------------------------------------------------------
// Happy-path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_deprecate_makes_old_an_alias_of_new() {
    let server = setup_test_server().await;
    let old = format!("test-deprecate-old-{}", uuid::Uuid::new_v4());
    let new = format!("test-deprecate-new-{}", uuid::Uuid::new_v4());

    create_model(&server, &old, &[]).await;
    create_model(&server, &new, &[]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({
            "modelId": old,
            "successorModelId": new,
            "changeReason": "e2e test"
        }),
    )
    .await;
    assert_eq!(
        resp.status_code(),
        200,
        "Deprecation should succeed: {}",
        resp.text()
    );

    let body: DeprecateModelResponse = resp.json();
    assert_eq!(body.deprecated.model_id, old);
    assert_eq!(body.successor.model_id, new);
    assert!(
        body.successor.metadata.aliases.contains(&old),
        "successor should now alias the deprecated id, got: {:?}",
        body.successor.metadata.aliases
    );
    assert_eq!(
        body.aliases_carried, 0,
        "no inbound aliases on the deprecated model — nothing to carry"
    );
}

#[tokio::test]
async fn test_deprecate_hides_old_from_admin_list_by_default() {
    let server = setup_test_server().await;
    let old = format!("test-deprecate-hide-{}", uuid::Uuid::new_v4());
    let new = format!("test-deprecate-hide-new-{}", uuid::Uuid::new_v4());
    create_model(&server, &old, &[]).await;
    create_model(&server, &new, &[]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({ "modelId": old, "successorModelId": new }),
    )
    .await;
    assert_eq!(resp.status_code(), 200);

    // Default listing (active only) should NOT contain the deprecated model.
    let active_list_resp = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    let active: AdminModelListResponse =
        serde_json::from_str(&active_list_resp.text()).expect("parse list");
    assert!(
        !active.models.iter().any(|m| m.model_id == old),
        "deprecated model should be hidden from active listing"
    );
    assert!(
        active.models.iter().any(|m| m.model_id == new),
        "successor should remain visible"
    );

    // include_inactive=true should still surface the deprecated model.
    let all_list_resp = server
        .get("/v1/admin/models?limit=500&include_inactive=true")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    let all: AdminModelListResponse =
        serde_json::from_str(&all_list_resp.text()).expect("parse list");
    let deprecated_entry = all
        .models
        .iter()
        .find(|m| m.model_id == old)
        .expect("deprecated model should appear when include_inactive=true");
    assert!(
        !deprecated_entry.is_active,
        "deprecated model should have isActive=false"
    );
}

#[tokio::test]
async fn test_deprecate_carries_inbound_aliases() {
    let server = setup_test_server().await;
    let old = format!("test-deprecate-carry-{}", uuid::Uuid::new_v4());
    let new = format!("test-deprecate-carry-new-{}", uuid::Uuid::new_v4());
    let inbound_alias_a = format!("legacy-a-{}", uuid::Uuid::new_v4());
    let inbound_alias_b = format!("legacy-b-{}", uuid::Uuid::new_v4());

    create_model(&server, &old, &[&inbound_alias_a, &inbound_alias_b]).await;
    create_model(&server, &new, &[]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({ "modelId": old, "successorModelId": new }),
    )
    .await;
    assert_eq!(
        resp.status_code(),
        200,
        "Deprecation should succeed: {}",
        resp.text()
    );
    let body: DeprecateModelResponse = resp.json();

    assert_eq!(
        body.aliases_carried, 2,
        "two pre-existing inbound aliases should be carried over"
    );

    // Successor should now own all three: the deprecated canonical name +
    // the two carried-over aliases.
    let succ_aliases = &body.successor.metadata.aliases;
    assert!(succ_aliases.contains(&old), "missing canonical {old}");
    assert!(
        succ_aliases.contains(&inbound_alias_a),
        "missing {inbound_alias_a}"
    );
    assert!(
        succ_aliases.contains(&inbound_alias_b),
        "missing {inbound_alias_b}"
    );
}

#[tokio::test]
async fn test_deprecate_preserves_successor_existing_aliases() {
    let server = setup_test_server().await;
    let old = format!("test-deprecate-preserve-{}", uuid::Uuid::new_v4());
    let new = format!("test-deprecate-preserve-new-{}", uuid::Uuid::new_v4());
    let pre_existing_alias = format!("succ-existing-alias-{}", uuid::Uuid::new_v4());

    create_model(&server, &old, &[]).await;
    create_model(&server, &new, &[&pre_existing_alias]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({ "modelId": old, "successorModelId": new }),
    )
    .await;
    assert_eq!(resp.status_code(), 200);

    let body: DeprecateModelResponse = resp.json();
    let aliases = &body.successor.metadata.aliases;
    assert!(
        aliases.contains(&pre_existing_alias),
        "pre-existing successor alias must not be wiped, got: {aliases:?}"
    );
    assert!(
        aliases.contains(&old),
        "deprecated id must be added as alias, got: {aliases:?}"
    );
}

// ---------------------------------------------------------------------------
// Validation / error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_deprecate_self_target_rejected() {
    let server = setup_test_server().await;
    let m = format!("test-deprecate-self-{}", uuid::Uuid::new_v4());
    create_model(&server, &m, &[]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({ "modelId": m, "successorModelId": m }),
    )
    .await;
    assert_eq!(resp.status_code(), 400);
    let err: ErrorResponse = resp.json();
    // ErrorDetail's serde rename for the discriminator field is `type`,
    // accessed via the Rust field name `r#type`.
    assert_eq!(err.error.r#type, "invalid_request");
}

#[tokio::test]
async fn test_deprecate_empty_id_rejected() {
    let server = setup_test_server().await;

    let resp = deprecate(
        &server,
        serde_json::json!({ "modelId": "", "successorModelId": "something" }),
    )
    .await;
    assert_eq!(resp.status_code(), 400);
}

#[tokio::test]
async fn test_deprecate_unknown_deprecated_returns_404() {
    let server = setup_test_server().await;
    let new = format!("test-deprecate-known-{}", uuid::Uuid::new_v4());
    create_model(&server, &new, &[]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({
            "modelId": "definitely-does-not-exist",
            "successorModelId": new,
        }),
    )
    .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn test_deprecate_unknown_successor_returns_404() {
    let server = setup_test_server().await;
    let old = format!("test-deprecate-known-old-{}", uuid::Uuid::new_v4());
    create_model(&server, &old, &[]).await;

    let resp = deprecate(
        &server,
        serde_json::json!({
            "modelId": old,
            "successorModelId": "definitely-does-not-exist",
        }),
    )
    .await;
    assert_eq!(resp.status_code(), 404);
}

#[tokio::test]
async fn test_deprecate_inactive_successor_returns_404() {
    let server = setup_test_server().await;
    let old = format!("test-deprecate-inactive-old-{}", uuid::Uuid::new_v4());
    let new = format!("test-deprecate-inactive-new-{}", uuid::Uuid::new_v4());
    create_model(&server, &old, &[]).await;
    create_model(&server, &new, &[]).await;

    // Deactivate the would-be successor first.
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        new.clone(),
        serde_json::from_value(serde_json::json!({ "isActive": false })).unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let resp = deprecate(
        &server,
        serde_json::json!({ "modelId": old, "successorModelId": new }),
    )
    .await;
    assert_eq!(
        resp.status_code(),
        404,
        "inactive successor should fail with 404, got: {} {}",
        resp.status_code(),
        resp.text()
    );
}
