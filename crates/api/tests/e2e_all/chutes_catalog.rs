// Regression tests for the Chutes (pinned, attested) catalog wiring:
//   1. an admin PATCH carrying `provider_type` must NOT tear down a pinned
//      provider (which the inference-url/external re-registration never restores).
//   2. startup catalog seeding must not clobber a row an operator created/activated
//      concurrently (INSERT ... ON CONFLICT DO NOTHING).

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use std::sync::Arc;

/// Blocking #1 (round-9): a PATCH with `provider_type: "chutes"` previously hit
/// the admin handler's unregister loop (`has_type_change`) and removed the pinned
/// Chutes provider, which the inference-url/external re-registration below never
/// restores — leaving an active catalog row with no serving provider until
/// restart. The `is_pinned` guard must keep the pinned provider registered.
#[tokio::test]
async fn admin_patch_with_provider_type_keeps_pinned_provider() {
    let (server, pool, _mock, _db) = setup_test_server_with_pool().await;
    let model = format!("zai-org/GLM-5.1-TEE-pin-{}", uuid::Uuid::new_v4());

    // Register a pinned provider exactly as init_inference_providers does for Chutes.
    pool.register_pinned_secondary_provider(
        model.clone(),
        Arc::new(inference_providers::mock::MockProvider::new()),
    )
    .await;
    assert!(pool.has_provider(&model).await);
    assert!(pool.is_pinned(&model));

    // The activation/update PATCH an operator sends: carries providerType=chutes.
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        model.clone(),
        serde_json::from_value(serde_json::json!({
            "inputCostPerToken":  { "amount": 1_000_000, "currency": "USD" },
            "outputCostPerToken": { "amount": 2_000_000, "currency": "USD" },
            "modelDisplayName":   "Chutes Pin Test",
            "modelDescription":   "Synthetic pinned model for the admin-PATCH regression",
            "contextLength":      4096,
            "providerType":       "chutes",
            "isActive":           true,
        }))
        .unwrap(),
    );
    let _ = admin_batch_upsert_models(&server, batch, get_session_id()).await;

    assert!(
        pool.has_provider(&model).await,
        "pinned Chutes provider must survive an admin PATCH carrying provider_type"
    );
    assert!(pool.is_pinned(&model));
}

/// Blocking #2 (round-9): `ensure_chutes_catalog_row` uses `seed_model_if_absent`
/// (INSERT ... ON CONFLICT DO NOTHING). If an operator creates/activates the model
/// concurrently with startup, the seed must NOT clobber the operator's row back to
/// the inactive, zero-priced seed defaults.
#[tokio::test]
async fn seed_model_if_absent_does_not_clobber_existing() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = database::repositories::ModelRepository::new(database.pool().clone());
    let model = format!("zai-org/GLM-5.1-TEE-seed-{}", uuid::Uuid::new_v4());

    let seed = || database::models::UpdateModelPricingRequest {
        model_display_name: Some("Seed".to_string()),
        model_description: Some("seed".to_string()),
        context_length: Some(128_000),
        is_active: Some(false),
        attestation_supported: Some(true),
        provider_type: Some("chutes".to_string()),
        ..Default::default()
    };

    // Fresh name -> seeds an INACTIVE row.
    let inserted = repo
        .seed_model_if_absent(&model, &seed())
        .await
        .unwrap()
        .expect("fresh model should be seeded");
    assert!(!inserted.is_active, "a freshly seeded row must be inactive");

    // Operator activates + prices the model.
    repo.upsert_model_pricing(
        &model,
        &database::models::UpdateModelPricingRequest {
            is_active: Some(true),
            input_cost_per_token: Some(1_234),
            output_cost_per_token: Some(5_678),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // A racing/duplicate seed must be a no-op (ON CONFLICT DO NOTHING) and leave
    // the operator's active, priced row completely untouched.
    let again = repo.seed_model_if_absent(&model, &seed()).await.unwrap();
    assert!(
        again.is_none(),
        "an existing row must not be re-seeded (ON CONFLICT DO NOTHING returns no row)"
    );

    let row = repo
        .get_by_internal_name(&model)
        .await
        .unwrap()
        .expect("row must still exist");
    assert!(
        row.is_active,
        "is_active must remain true (not clobbered back to the seed's false)"
    );
    assert_eq!(
        row.input_cost_per_token, 1_234,
        "input pricing must be preserved"
    );
    assert_eq!(
        row.output_cost_per_token, 5_678,
        "output pricing must be preserved"
    );
}
