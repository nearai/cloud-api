// E2E tests for admin list models endpoint

use crate::common::*;
use api::models::{AdminModelListResponse, BatchUpdateModelApiRequest};

// ============================================
// List Models Tests
// ============================================

#[tokio::test]
async fn test_admin_list_models_response_structure() {
    let server = setup_test_server().await;

    // List models and verify response structure
    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    println!("Response status: {}", response.status_code());
    println!("Response body: {}", response.text());

    assert_eq!(
        response.status_code(),
        200,
        "Should successfully list models"
    );

    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse AdminModelListResponse");

    // Verify structure
    assert!(list_response.limit > 0, "Limit should be positive");
    assert!(list_response.offset >= 0, "Offset should be non-negative");
    assert!(list_response.total >= 0, "Total should be non-negative");
    assert!(
        list_response.models.len() as i64 <= list_response.limit,
        "Models count should not exceed limit"
    );

    println!("✅ Admin list models response structure is valid");
}

#[tokio::test]
async fn test_admin_list_models_with_models() {
    let server = setup_test_server().await;

    // Create a test model
    let model_name = format!("test-model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model",
            "modelDescription": "A test model for e2e testing",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true,
            "inputModalities": ["text", "image"],
            "outputModalities": ["text"]
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Should have created 1 model");

    // List models
    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);

    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    println!("Models found: {}", list_response.total);

    // Find our test model
    let our_model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name);

    assert!(our_model.is_some(), "Should find the test model in list");
    let model = our_model.unwrap();

    // Verify model fields
    assert_eq!(model.model_id, model_name);
    assert_eq!(model.input_cost_per_token.amount, 1000000);
    assert_eq!(model.output_cost_per_token.amount, 2000000);
    assert!(model.is_active, "Model should be active");
    assert_eq!(model.metadata.model_display_name, "Test Model");
    assert_eq!(model.metadata.context_length, 4096);

    // Verify architecture/modalities
    let architecture = model
        .metadata
        .architecture
        .as_ref()
        .expect("Model should have architecture");
    assert_eq!(
        architecture.input_modalities,
        vec!["text", "image"],
        "Input modalities should match"
    );
    assert_eq!(
        architecture.output_modalities,
        vec!["text"],
        "Output modalities should match"
    );

    println!("✅ Admin list models with models works correctly");
}

#[tokio::test]
async fn test_admin_upsert_is_ready_and_deprecation_date_round_trip() {
    let server = setup_test_server().await;

    // Create a model with the two OpenRouter fields set.
    let model_name = format!("test-isready-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000, "currency": "USD" },
            "modelDisplayName": "Test is_ready Model",
            "modelDescription": "Round-trips is_ready and deprecation_date",
            "contextLength": 4096,
            "isActive": true,
            "isReady": true,
            "deprecationDate": "2030-01-01"
        }))
        .unwrap(),
    );

    let updated = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let model = updated
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("upsert should return our model");

    assert_eq!(
        model.metadata.is_ready,
        Some(true),
        "is_ready should round-trip verbatim"
    );
    // Per the OpenRouter provider spec, "Date-only values default to 13:00 UTC
    // on that date" and explicit times use the UTC-hour form
    // (`YYYY-MM-DDTHH:00:00Z`). A bare date therefore normalizes to 13:00 UTC
    // and serializes as `2030-01-01T13:00:00Z`.
    let dep = model
        .metadata
        .deprecation_date
        .as_ref()
        .expect("deprecation_date should be present");
    assert_eq!(
        dep, "2030-01-01T13:00:00Z",
        "date-only deprecation_date should default to 13:00 UTC (OpenRouter spec), got {dep}"
    );

    // Verify the same value round-trips through the public GET /v1/models.
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let listed = list_models(&server, api_key.clone()).await;
    let public = listed
        .data
        .iter()
        .find(|m| m.id == model_name)
        .expect("model should appear in GET /v1/models");
    assert_eq!(public.is_ready, Some(true));
    assert_eq!(
        public.deprecation_date.as_deref(),
        Some("2030-01-01T13:00:00Z"),
        "GET /v1/models must emit the OpenRouter-compatible 13:00 UTC value"
    );

    // Now set is_ready to false and supply an explicit UTC-hour datetime.
    let mut batch2 = BatchUpdateModelApiRequest::new();
    batch2.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "isReady": false,
            "deprecationDate": "2031-06-15T15:00:00Z"
        }))
        .unwrap(),
    );
    let updated2 = admin_batch_upsert_models(&server, batch2, get_session_id()).await;
    let model2 = updated2
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("update should return our model");
    assert_eq!(model2.metadata.is_ready, Some(false));
    assert_eq!(
        model2.metadata.deprecation_date.as_deref(),
        Some("2031-06-15T15:00:00Z"),
        "explicit datetime should serialize in UTC-hour form (OpenRouter spec)"
    );

    // A finer-than-hour explicit time is normalized down to the whole UTC hour
    // (the spec only models hour precision).
    let mut batch3 = BatchUpdateModelApiRequest::new();
    batch3.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "deprecationDate": "2031-06-15T15:47:33Z"
        }))
        .unwrap(),
    );
    let updated3 = admin_batch_upsert_models(&server, batch3, get_session_id()).await;
    let model3 = updated3
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("update should return our model");
    assert_eq!(
        model3.metadata.deprecation_date.as_deref(),
        Some("2031-06-15T15:00:00Z"),
        "sub-hour precision should truncate to the top of the UTC hour"
    );
}

#[tokio::test]
async fn test_admin_clear_deprecation_date_and_is_ready() {
    let server = setup_test_server().await;

    // Create a model with both fields set.
    let model_name = format!("test-clear-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000, "currency": "USD" },
            "modelDisplayName": "Clearable Model",
            "modelDescription": "Tri-state clear semantics",
            "contextLength": 4096,
            "isActive": true,
            "isReady": false,
            "deprecationDate": "2030-01-01"
        }))
        .unwrap(),
    );
    let created = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    let created_model = created
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("upsert should return our model");
    assert_eq!(created_model.metadata.is_ready, Some(false));
    assert_eq!(
        created_model.metadata.deprecation_date.as_deref(),
        Some("2030-01-01T13:00:00Z")
    );

    // Explicit JSON null clears both fields back to "unset".
    let mut clear_batch = BatchUpdateModelApiRequest::new();
    clear_batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "isReady": null,
            "deprecationDate": null
        }))
        .unwrap(),
    );
    let cleared = admin_batch_upsert_models(&server, clear_batch, get_session_id()).await;
    let cleared_model = cleared
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("update should return our model");
    assert_eq!(
        cleared_model.metadata.is_ready, None,
        "explicit null should clear is_ready"
    );
    assert_eq!(
        cleared_model.metadata.deprecation_date, None,
        "explicit null should clear deprecation_date"
    );

    // The fields must also be absent from the public GET /v1/models response.
    let org = setup_org_with_credits(&server, 10_000_000_000i64).await;
    let api_key = get_api_key_for_org(&server, org.id).await;
    let listed = list_models(&server, api_key).await;
    let public = listed
        .data
        .iter()
        .find(|m| m.id == model_name)
        .expect("model should appear in GET /v1/models");
    assert_eq!(
        public.deprecation_date, None,
        "cleared deprecation_date must disappear from GET /v1/models"
    );
    assert_eq!(
        public.is_ready, None,
        "cleared is_ready must disappear from GET /v1/models"
    );

    // An omitted field after a set must NOT clear it (regression guard for the
    // tri-state: absent != null).
    let mut set_batch = BatchUpdateModelApiRequest::new();
    set_batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "deprecationDate": "2032-03-03"
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, set_batch, get_session_id()).await;

    let mut noop_batch = BatchUpdateModelApiRequest::new();
    noop_batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "isActive": true
        }))
        .unwrap(),
    );
    let after_noop = admin_batch_upsert_models(&server, noop_batch, get_session_id()).await;
    let after_noop_model = after_noop
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("update should return our model");
    assert_eq!(
        after_noop_model.metadata.deprecation_date.as_deref(),
        Some("2032-03-03T13:00:00Z"),
        "omitting deprecationDate must preserve the existing value"
    );
}

#[tokio::test]
async fn test_admin_upsert_rejects_invalid_deprecation_date() {
    let server = setup_test_server().await;

    let model_name = format!("test-baddep-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name,
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000, "currency": "USD" },
            "modelDisplayName": "Bad deprecation date",
            "modelDescription": "Should be rejected",
            "contextLength": 4096,
            "deprecationDate": "not-a-date"
        }))
        .unwrap(),
    );

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&batch)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Invalid deprecationDate should be rejected with 400"
    );
    assert!(
        response.text().contains("deprecationDate"),
        "error should mention deprecationDate, got: {}",
        response.text()
    );
}

#[tokio::test]
async fn test_admin_list_models_include_inactive() {
    let server = setup_test_server().await;

    // Create an active model
    let active_model_name = format!("active-model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        active_model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Active Model",
            "modelDescription": "An active model",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Create an inactive model
    let inactive_model_name = format!("inactive-model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        inactive_model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Inactive Model",
            "modelDescription": "An inactive model",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": false
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // List models without include_inactive (default - should only show active)
    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let active_found = list_response
        .models
        .iter()
        .any(|m| m.model_id == active_model_name);
    let inactive_found = list_response
        .models
        .iter()
        .any(|m| m.model_id == inactive_model_name);

    assert!(active_found, "Should find active model in default list");
    assert!(
        !inactive_found,
        "Should NOT find inactive model in default list"
    );

    // List models with include_inactive=true
    let response = server
        .get("/v1/admin/models?include_inactive=true&limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let active_found = list_response
        .models
        .iter()
        .any(|m| m.model_id == active_model_name);
    let inactive_found = list_response
        .models
        .iter()
        .any(|m| m.model_id == inactive_model_name);

    assert!(
        active_found,
        "Should find active model when include_inactive=true"
    );
    assert!(
        inactive_found,
        "Should find inactive model when include_inactive=true"
    );

    // Verify the inactive model has is_active=false
    let inactive_model = list_response
        .models
        .iter()
        .find(|m| m.model_id == inactive_model_name)
        .unwrap();
    assert!(
        !inactive_model.is_active,
        "Inactive model should have is_active=false"
    );

    println!("✅ Admin list models with include_inactive works correctly");
}

#[tokio::test]
async fn test_admin_list_models_pagination() {
    let server = setup_test_server().await;

    // Create multiple models to test pagination
    for i in 0..5 {
        let model_name = format!("pagination-model-{}-{}", i, uuid::Uuid::new_v4());
        let mut batch = BatchUpdateModelApiRequest::new();
        batch.insert(
            model_name,
            serde_json::from_value(serde_json::json!({
                "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
                "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
                "modelDisplayName": format!("Pagination Model {}", i),
                "modelDescription": "A model for pagination testing",
                "contextLength": 4096,
                "verifiable": false,
                "isActive": true
            }))
            .unwrap(),
        );
        admin_batch_upsert_models(&server, batch, get_session_id()).await;
    }

    // Test limit
    let response = server
        .get("/v1/admin/models?limit=2")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(list_response.limit, 2, "Limit should be 2");
    assert!(
        list_response.models.len() <= 2,
        "Should return at most 2 models"
    );

    // Test offset
    let response = server
        .get("/v1/admin/models?limit=2&offset=2")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert_eq!(list_response.limit, 2, "Limit should be 2");
    assert_eq!(list_response.offset, 2, "Offset should be 2");

    println!("✅ Admin list models pagination works correctly");
}

#[tokio::test]
async fn test_admin_list_models_has_timestamps() {
    let server = setup_test_server().await;

    // Create a model
    let model_name = format!("timestamp-model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Timestamp Model",
            "modelDescription": "A model for timestamp testing",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // List models (use high limit to find our model in shared DB)
    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    // Find our model
    let model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("Should find our model");

    // Verify timestamps exist and are valid
    // created_at should be close to now
    let now = chrono::Utc::now();
    let created_diff = now.signed_duration_since(model.created_at);
    assert!(
        created_diff.num_seconds() < 60,
        "created_at should be recent"
    );

    // updated_at should be >= created_at
    assert!(
        model.updated_at >= model.created_at,
        "updated_at should be >= created_at"
    );

    println!("✅ Admin list models has valid timestamps");
}

// ============================================
// Authorization Tests
// ============================================

#[tokio::test]
async fn test_admin_list_models_unauthorized() {
    let server = setup_test_server().await;

    // Try to list models without auth
    let response = server.get("/v1/admin/models?limit=500").await;

    assert_eq!(
        response.status_code(),
        401,
        "Should return 401 without auth"
    );

    println!("✅ Admin list models correctly requires authentication");
}

#[tokio::test]
async fn test_admin_list_models_invalid_pagination() {
    let server = setup_test_server().await;

    // Test negative offset
    let response = server
        .get("/v1/admin/models?offset=-1")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should return 400 for negative offset"
    );

    // Test negative limit
    let response = server
        .get("/v1/admin/models?limit=-1")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Should return 400 for negative limit"
    );

    println!("✅ Admin list models handles invalid pagination correctly");
}

// ============================================
// Soft Delete Integration Tests
// ============================================

#[tokio::test]
async fn test_admin_list_models_after_soft_delete() {
    let server = setup_test_server().await;

    // Create a model
    let model_name = format!("delete-test-model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Delete Test Model",
            "modelDescription": "A model for soft delete testing",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Verify model exists in active list
    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert!(
        list_response
            .models
            .iter()
            .any(|m| m.model_id == model_name),
        "Model should be in active list"
    );

    // Soft delete the model (URL-encode in case of special chars)
    let encoded_model_name =
        url::form_urlencoded::byte_serialize(model_name.as_bytes()).collect::<String>();
    let delete_response = server
        .delete(format!("/v1/admin/models/{encoded_model_name}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(delete_response.status_code(), 204, "Delete should succeed");

    // Model should NOT be in default active-only list
    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    assert!(
        !list_response
            .models
            .iter()
            .any(|m| m.model_id == model_name),
        "Deleted model should NOT be in active list"
    );

    // Model SHOULD be in include_inactive list
    let response = server
        .get("/v1/admin/models?include_inactive=true&limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;

    assert_eq!(response.status_code(), 200);
    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let deleted_model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name);
    assert!(
        deleted_model.is_some(),
        "Deleted model should be in include_inactive list"
    );
    assert!(
        !deleted_model.unwrap().is_active,
        "Deleted model should have is_active=false"
    );

    println!("✅ Admin list models handles soft delete correctly");
}

// ============================================
// OpenRouter `datacenters` field
// ============================================

/// Setting `datacenters` via the admin upsert should persist and surface as
/// `[{ "country_code": "US" }, ...]` on the admin list endpoint.
#[tokio::test]
async fn test_admin_list_models_datacenters_round_trip() {
    let server = setup_test_server().await;

    let model_name = format!("datacenters-model-{}", uuid::Uuid::new_v4());
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Datacenters Model",
            "modelDescription": "A model with datacenters set",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true,
            "datacenters": [{ "country_code": "US" }, { "country_code": "FR" }]
        }))
        .unwrap(),
    );
    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let response = server
        .get("/v1/admin/models?limit=500")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(response.status_code(), 200);

    let list_response: AdminModelListResponse =
        serde_json::from_str(&response.text()).expect("Failed to parse response");

    let model = list_response
        .models
        .iter()
        .find(|m| m.model_id == model_name)
        .expect("Should find the datacenters test model");

    let datacenters = model
        .metadata
        .datacenters
        .as_ref()
        .expect("metadata should carry datacenters");
    let codes: Vec<&str> = datacenters
        .iter()
        .map(|dc| dc.country_code.as_str())
        .collect();
    assert_eq!(codes, vec!["US", "FR"], "Datacenters should round-trip");

    println!("✅ Admin datacenters round-trip works correctly");
}

/// `datacenters` country codes must be 2-letter uppercase ISO 3166 Alpha-2.
/// Garbage codes are rejected at the admin write path with a 400.
#[tokio::test]
async fn test_admin_upsert_rejects_invalid_datacenters() {
    let server = setup_test_server().await;

    let model_name = format!("bad-datacenters-{}", uuid::Uuid::new_v4());
    let body = serde_json::json!({
        model_name: {
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Bad Datacenters Model",
            "modelDescription": "Has an invalid country code",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true,
            "datacenters": [{ "country_code": "usa" }]
        }
    });

    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await;

    assert_eq!(
        response.status_code(),
        400,
        "Invalid ISO 3166 Alpha-2 country code should be rejected"
    );

    // Lowercase two-letter codes must also be rejected (must be uppercase).
    let model_name = format!("bad-datacenters-lc-{}", uuid::Uuid::new_v4());
    let body = serde_json::json!({
        model_name: {
            "inputCostPerToken": { "amount": 1000000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2000000, "currency": "USD" },
            "modelDisplayName": "Bad Datacenters Model",
            "modelDescription": "Has a lowercase country code",
            "contextLength": 4096,
            "verifiable": false,
            "isActive": true,
            "datacenters": [{ "country_code": "us" }]
        }
    });
    let response = server
        .patch("/v1/admin/models")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await;
    assert_eq!(
        response.status_code(),
        400,
        "Lowercase country code should be rejected"
    );

    println!("✅ Admin upsert rejects invalid datacenters");
}
