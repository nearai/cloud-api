// E2E tests for admin list models endpoint
mod common;

use api::models::{AdminModelListResponse, BatchUpdateModelApiRequest};
use common::*;

// ============================================
// List Models Tests
// ============================================

#[tokio::test]
async fn test_admin_list_models_response_structure() {
    let (server, _guard) = setup_test_server().await;

    // List models and verify response structure
    let response = server
        .get("/v1/admin/models")
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
    let (server, _guard) = setup_test_server().await;

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
        .get("/v1/admin/models")
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
async fn test_admin_list_models_include_inactive() {
    let (server, _guard) = setup_test_server().await;

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
        .get("/v1/admin/models")
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
        .get("/v1/admin/models?include_inactive=true")
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
    let (server, _guard) = setup_test_server().await;

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
    let (server, _guard) = setup_test_server().await;

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

    // List models
    let response = server
        .get("/v1/admin/models")
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
    let (server, _guard) = setup_test_server().await;

    // Try to list models without auth
    let response = server.get("/v1/admin/models").await;

    assert_eq!(
        response.status_code(),
        401,
        "Should return 401 without auth"
    );

    println!("✅ Admin list models correctly requires authentication");
}

#[tokio::test]
async fn test_admin_list_models_invalid_pagination() {
    let (server, _guard) = setup_test_server().await;

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
    let (server, _guard) = setup_test_server().await;

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
        .get("/v1/admin/models")
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
        .get("/v1/admin/models")
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
        .get("/v1/admin/models?include_inactive=true")
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
