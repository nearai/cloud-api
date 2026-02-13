// Tests for model history recording with user audit tracking
mod common;

use common::*;
use std::collections::HashMap;

/// Helper function to query model history directly from database
async fn get_model_history_from_db(
    server: &axum_test::TestServer,
    model_name: &str,
) -> Vec<(
    uuid::Uuid,
    Option<uuid::Uuid>,
    Option<String>,
    Option<String>,
    bool,
)> {
    // We'll use the API endpoint to get history instead of direct DB query
    // This is more realistic for E2E testing
    let response = server
        .get(format!("/v1/admin/models/{model_name}/history").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200, "Failed to get model history");

    let history_response = response.json::<api::models::ModelHistoryResponse>();

    // Map to (id, changed_by_user_id, changed_by_user_email, change_reason, is_active)
    history_response
        .history
        .iter()
        .map(|h| {
            (
                uuid::Uuid::parse_str(&h.id).unwrap(),
                h.changed_by_user_id
                    .as_ref()
                    .and_then(|id| uuid::Uuid::parse_str(id).ok()),
                h.changed_by_user_email.clone(),
                h.change_reason.clone(),
                h.is_active,
            )
        })
        .collect()
}

/// Test 1: Creating a new model should create a history record
#[tokio::test]
async fn test_upsert_model_creates_history_record() {
    let server = setup_test_server().await;

    let mut batch = HashMap::new();
    let model_name = format!("test-model-history-1-{}", uuid::Uuid::new_v4());

    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model History",
            "modelDescription": "Testing history recording",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true,
            "changeReason": "Initial model creation"
        }))
        .unwrap(),
    );

    // Upsert the model
    let result = admin_batch_upsert_models(&server, batch, get_session_id()).await;
    assert_eq!(result.len(), 1);

    // Get history
    let history = get_model_history_from_db(&server, &model_name).await;

    // Should have exactly 1 history record
    assert_eq!(
        history.len(),
        1,
        "Expected 1 history record after initial upsert"
    );

    let (id, user_id, user_email, reason, is_active) = &history[0];

    // Verify the record
    assert!(!id.is_nil(), "History record should have an ID");
    assert!(
        user_id.is_some(),
        "History record should have changed_by_user_id"
    );
    assert!(
        user_email.is_some(),
        "History record should have changed_by_user_email"
    );
    assert_eq!(
        user_email.as_ref().unwrap(),
        "admin@test.com",
        "User email should match mock user"
    );
    assert_eq!(
        reason.as_ref().unwrap(),
        "Initial model creation",
        "Change reason should match request"
    );
    assert!(*is_active, "Model should be active");
}

/// Test 2: Updating a model should close previous history and create new record
#[tokio::test]
async fn test_second_upsert_closes_previous_history() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-history-2-{}", uuid::Uuid::new_v4());

    // First upsert - create the model
    let mut batch1 = HashMap::new();
    batch1.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model V1",
            "modelDescription": "Version 1",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true,
            "changeReason": "Initial creation"
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch1, get_session_id()).await;

    // Get initial history
    let history_v1 = get_model_history_from_db(&server, &model_name).await;
    assert_eq!(
        history_v1.len(),
        1,
        "Should have 1 history record after first upsert"
    );

    // Second upsert - update pricing
    let mut batch2 = HashMap::new();
    batch2.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 5000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 10000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model V2",
            "modelDescription": "Version 2 - pricing updated",
            "contextLength": 8192,
            "verifiable": true,
            "isActive": true,
            "changeReason": "Pricing update"
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch2, get_session_id()).await;

    // Get updated history
    let history_v2 = get_model_history_from_db(&server, &model_name).await;

    // Should now have 2 history records
    assert_eq!(
        history_v2.len(),
        2,
        "Should have 2 history records after second upsert"
    );

    // Both records should have user tracking
    for (_, user_id, user_email, _, _) in &history_v2 {
        assert!(user_id.is_some(), "All history records should have user ID");
        assert!(
            user_email.is_some(),
            "All history records should have user email"
        );
        assert_eq!(
            user_email.as_ref().unwrap(),
            "admin@test.com",
            "All records should have admin email"
        );
    }

    // Verify change reasons
    assert_eq!(
        history_v2[0].3.as_ref().unwrap(),
        "Pricing update",
        "Most recent record should have update reason"
    );
    assert_eq!(
        history_v2[1].3.as_ref().unwrap(),
        "Initial creation",
        "Previous record should have initial reason"
    );
}

/// Test 3: Soft delete should create history record with is_active=false
#[tokio::test]
async fn test_soft_delete_creates_history_record() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-delete-1-{}", uuid::Uuid::new_v4());

    // Create a model
    let mut batch = HashMap::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model Delete",
            "modelDescription": "Will be deleted",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Verify created
    let history_before = get_model_history_from_db(&server, &model_name).await;
    assert_eq!(history_before.len(), 1);
    assert!(history_before[0].4, "Model should be active");

    // Soft delete the model
    let delete_response = server
        .delete(format!("/v1/admin/models/{model_name}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        delete_response.status_code(),
        204,
        "Soft delete should succeed"
    );

    // Get history after delete
    let history_after = get_model_history_from_db(&server, &model_name).await;

    // Should now have 2 records
    assert_eq!(
        history_after.len(),
        2,
        "Should have 2 history records after delete (original + deletion record)"
    );

    // Most recent record should have is_active=false
    assert!(
        !history_after[0].4,
        "Latest history should show is_active=false"
    );

    // Should have change_reason for deletion
    assert!(
        history_after[0].3.is_some(),
        "Delete record should have change_reason"
    );
    assert_eq!(
        history_after[0].3.as_ref().unwrap(),
        "Model soft deleted",
        "Delete record should have appropriate reason"
    );

    // Original record should still be active
    assert!(
        history_after[1].4,
        "Original history should show is_active=true"
    );
}

/// Test 4: Verify user tracking is correctly recorded
#[tokio::test]
async fn test_user_tracking_in_history() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-user-tracking-{}", uuid::Uuid::new_v4());

    // Create model with explicit change reason
    let mut batch = HashMap::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "User Tracking Test",
            "modelDescription": "Testing user tracking",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true,
            "changeReason": "Testing user audit"
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    let history = get_model_history_from_db(&server, &model_name).await;
    assert_eq!(history.len(), 1);

    let (_, user_id, user_email, reason, _) = &history[0];

    // Verify user tracking
    assert!(user_id.is_some(), "User ID should be recorded");
    assert_eq!(
        user_id.unwrap().to_string(),
        MOCK_USER_ID,
        "User ID should be admin user"
    );
    assert_eq!(
        user_email.as_ref().unwrap(),
        "admin@test.com",
        "User email should be recorded"
    );
    assert_eq!(
        reason.as_ref().unwrap(),
        "Testing user audit",
        "Change reason should be recorded"
    );
}

/// Test 5: Multiple updates should show progression in history
#[tokio::test]
async fn test_history_progression_multiple_updates() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-progression-{}", uuid::Uuid::new_v4());

    let update_reasons = [
        "Initial creation",
        "Pricing adjustment v1",
        "Pricing adjustment v2",
        "Context length increase",
    ];

    for (i, reason) in update_reasons.iter().enumerate() {
        let mut batch = HashMap::new();
        batch.insert(
            model_name.clone(),
            serde_json::from_value(serde_json::json!({
                "inputCostPerToken": {
                    "amount": (1000 * (i + 1)) as i64,
                    "currency": "USD"
                },
                "outputCostPerToken": {
                    "amount": (2000 * (i + 1)) as i64,
                    "currency": "USD"
                },
                "modelDisplayName": format!("Model V{}", i + 1),
                "modelDescription": format!("Version {}", i + 1),
                "contextLength": 4096 * (i + 1) as i32,
                "verifiable": true,
                "isActive": true,
                "changeReason": reason
            }))
            .unwrap(),
        );

        admin_batch_upsert_models(&server, batch, get_session_id()).await;
    }

    let history = get_model_history_from_db(&server, &model_name).await;

    // Should have exactly 4 records
    assert_eq!(
        history.len(),
        4,
        "Should have {} history records",
        update_reasons.len()
    );

    // Verify each record has the correct reason (in reverse order due to DESC ordering)
    for (i, (_, user_id, user_email, reason, _)) in history.iter().enumerate() {
        let expected_reason = update_reasons[update_reasons.len() - 1 - i];
        assert_eq!(
            reason.as_ref().unwrap(),
            expected_reason,
            "Record {i} should have correct reason"
        );
        assert!(user_id.is_some(), "Record {i} should have user ID");
        assert_eq!(
            user_email.as_ref().unwrap(),
            "admin@test.com",
            "Record {i} should have correct user email"
        );
    }
}

/// Test 6: Soft delete with custom change_reason
#[tokio::test]
async fn test_soft_delete_with_custom_reason() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-delete-custom-{}", uuid::Uuid::new_v4());

    // Create a model
    let mut batch = HashMap::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model Delete Custom",
            "modelDescription": "Will be deleted with custom reason",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Soft delete with custom reason
    let delete_response = server
        .delete(format!("/v1/admin/models/{model_name}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .json(&serde_json::json!({
            "changeReason": "Replaced by newer model version"
        }))
        .await;

    assert_eq!(
        delete_response.status_code(),
        204,
        "Soft delete with custom reason should succeed"
    );

    // Get history and verify custom reason
    let history = get_model_history_from_db(&server, &model_name).await;
    assert_eq!(history.len(), 2, "Should have 2 history records");

    // Latest record should have the custom reason
    let (_, _, _, reason, is_active) = &history[0];
    assert!(!is_active, "Latest record should show is_active=false");
    assert_eq!(
        reason.as_ref().unwrap(),
        "Replaced by newer model version",
        "Should use provided custom reason"
    );
}

/// Test 7: Soft delete without change_reason (backward compatibility)
#[tokio::test]
async fn test_soft_delete_without_reason_backward_compatibility() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-delete-no-reason-{}", uuid::Uuid::new_v4());

    // Create a model
    let mut batch = HashMap::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "Test Model Delete No Reason",
            "modelDescription": "Will be deleted without reason",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Soft delete without providing any reason (backward compatible - empty body)
    let delete_response = server
        .delete(format!("/v1/admin/models/{model_name}").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(
        delete_response.status_code(),
        204,
        "Soft delete without reason should still work (backward compatible)"
    );

    // Get history and verify default reason is used
    let history = get_model_history_from_db(&server, &model_name).await;
    assert_eq!(history.len(), 2, "Should have 2 history records");

    // Latest record should have the default reason
    let (_, _, _, reason, is_active) = &history[0];
    assert!(!is_active, "Latest record should show is_active=false");
    assert_eq!(
        reason.as_ref().unwrap(),
        "Model soft deleted",
        "Should use default reason when none provided"
    );
}

/// Test 8: Model history should track input/output modalities
#[tokio::test]
async fn test_model_history_tracks_modalities() {
    let server = setup_test_server().await;
    let model_name = format!("test-model-modalities-{}", uuid::Uuid::new_v4());

    // Create a model with modalities
    let mut batch = HashMap::new();
    batch.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1000,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2000,
                "currency": "USD"
            },
            "modelDisplayName": "Multimodal Model",
            "modelDescription": "A model with text and image input",
            "contextLength": 4096,
            "verifiable": true,
            "isActive": true,
            "inputModalities": ["text", "image"],
            "outputModalities": ["text"],
            "changeReason": "Initial creation with modalities"
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch, get_session_id()).await;

    // Get history via API
    let response = server
        .get(format!("/v1/admin/models/{model_name}/history").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200, "Failed to get model history");
    let history_response = response.json::<api::models::ModelHistoryResponse>();

    assert_eq!(
        history_response.history.len(),
        1,
        "Should have 1 history record"
    );

    let history_entry = &history_response.history[0];

    // Verify modalities are recorded in history
    assert_eq!(
        history_entry.input_modalities.as_ref().unwrap(),
        &vec!["text".to_string(), "image".to_string()],
        "Input modalities should be recorded in history"
    );
    assert_eq!(
        history_entry.output_modalities.as_ref().unwrap(),
        &vec!["text".to_string()],
        "Output modalities should be recorded in history"
    );

    // Update the model with different modalities
    let mut batch2 = HashMap::new();
    batch2.insert(
        model_name.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken": {
                "amount": 1500,
                "currency": "USD"
            },
            "outputCostPerToken": {
                "amount": 2500,
                "currency": "USD"
            },
            "modelDisplayName": "Multimodal Model V2",
            "modelDescription": "Now supports audio input too",
            "contextLength": 8192,
            "verifiable": true,
            "isActive": true,
            "inputModalities": ["text", "image", "audio"],
            "outputModalities": ["text", "audio"],
            "changeReason": "Added audio modality"
        }))
        .unwrap(),
    );

    admin_batch_upsert_models(&server, batch2, get_session_id()).await;

    // Get updated history
    let response = server
        .get(format!("/v1/admin/models/{model_name}/history").as_str())
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .await;

    assert_eq!(response.status_code(), 200);
    let history_response = response.json::<api::models::ModelHistoryResponse>();

    assert_eq!(
        history_response.history.len(),
        2,
        "Should have 2 history records"
    );

    // Most recent entry (index 0) should have updated modalities
    let latest_entry = &history_response.history[0];
    assert_eq!(
        latest_entry.input_modalities.as_ref().unwrap(),
        &vec!["text".to_string(), "image".to_string(), "audio".to_string()],
        "Latest history should have updated input modalities"
    );
    assert_eq!(
        latest_entry.output_modalities.as_ref().unwrap(),
        &vec!["text".to_string(), "audio".to_string()],
        "Latest history should have updated output modalities"
    );

    // Previous entry (index 1) should have original modalities
    let previous_entry = &history_response.history[1];
    assert_eq!(
        previous_entry.input_modalities.as_ref().unwrap(),
        &vec!["text".to_string(), "image".to_string()],
        "Previous history should have original input modalities"
    );
    assert_eq!(
        previous_entry.output_modalities.as_ref().unwrap(),
        &vec!["text".to_string()],
        "Previous history should have original output modalities"
    );
}
