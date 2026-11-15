// Regression tests for the Chutes (pinned, attested) catalog wiring:
//   1. an admin PATCH carrying `provider_type` must NOT tear down a pinned
//      provider (which the inference-url/external re-registration never restores).
//   2. startup catalog seeding must not clobber a row an operator created/activated
//      concurrently (INSERT ... ON CONFLICT DO NOTHING).

use crate::common::*;

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
            max_output_length: Some(1024),
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

/// `ensure_attested_3p_catalog_row` with no seed and no existing row must not
/// create anything and must return `None` (3P-only models are out of scope).
#[tokio::test]
async fn ensure_catalog_row_without_seed_and_without_row_returns_none() {
    let (_server, database) = setup_test_server_with_database().await;
    let repo = database::repositories::ModelRepository::new(database.pool().clone());
    let model = format!("zai-org/no-seed-{}", uuid::Uuid::new_v4());

    let role = api::attested_3p_startup::ensure_attested_3p_catalog_row(
        &repo,
        inference_providers::ProviderSource::Tinfoil,
        &model,
        None,
    )
    .await;
    assert!(role.is_none());
    assert!(
        repo.get_by_internal_name(&model).await.unwrap().is_none(),
        "no row may be seeded when no seed is given"
    );
}
