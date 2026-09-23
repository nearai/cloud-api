#[allow(dead_code)]
mod support;

use database::models::RecordUsageRequest;
use database::models::UpdateOrganizationLimitsDbRequest;
use database::repositories::{
    ApiKeyRepository, OrganizationLimitsRepository, OrganizationServiceUsageRepository,
    OrganizationUsageRepository, PgAdmissionSnapshotRepository, RecordServiceUsageRequest,
};
use services::usage::admission::AdmissionSnapshotRepository;
use services::usage::InferenceType;
use std::time::Duration;
use support::{cleanup_usage_fixtures, insert_model, insert_org_fixture, test_pool};
use uuid::Uuid;

fn usage_request(
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    model_name: String,
    inference_id: Uuid,
    total_cost: i64,
) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id,
        workspace_id,
        api_key_id,
        model_id,
        model_name,
        input_tokens: 1,
        output_tokens: 0,
        input_cost: total_cost,
        output_cost: 0,
        total_cost,
        inference_type: InferenceType::ChatCompletion.as_str().to_string(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(inference_id),
        provider_request_id: Some(format!("snapshot-{inference_id}")),
        stop_reason: None,
        response_id: None,
        image_count: None,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        billing_details: None,
        service_tier: None,
        context_band: None,
        served_provider_tier: None,
        served_provider_type: None,
        served_via_fallback: false,
    }
}

async fn set_payment_limit(
    repository: &OrganizationLimitsRepository,
    organization_id: Uuid,
    amount: i64,
) -> anyhow::Result<()> {
    repository
        .update_limits(
            organization_id,
            &UpdateOrganizationLimitsDbRequest {
                spend_limit: amount,
                credit_type: "payment".to_string(),
                source: Some("snapshot-test".to_string()),
                currency: "USD".to_string(),
                changed_by: Some("snapshot-test".to_string()),
                change_reason: Some("snapshot regression".to_string()),
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn admission_snapshot_is_coherent_and_revisions_only_semantic_changes() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let other_org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "admission-snapshot").await?;
    let repository = PgAdmissionSnapshotRepository::new(pool.clone(), Duration::from_millis(500));
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let usage = OrganizationUsageRepository::new(pool.clone());
    let api_keys = ApiKeyRepository::new(pool.clone());

    // Organization creation normally creates a zero balance; explicitly exercise
    // the supported missing-balance state before the first usage write.
    pool.get()
        .await?
        .execute(
            "DELETE FROM organization_balance WHERE organization_id = $1",
            &[&org.org_id],
        )
        .await?;
    let initial = repository.load_organization(org.org_id).await?;
    assert_eq!(initial.revision, 0);
    assert_eq!(initial.total_spent, None);
    assert!(initial.limit.is_none());
    let initial_key = repository.load_key(org.org_id, org.api_key_a_id).await?;
    assert_eq!(initial_key.revision, 0);
    assert_eq!(initial_key.spend_limit, None);
    assert_eq!(initial_key.inference_spent, 0);

    set_payment_limit(&limits, org.org_id, 10).await?;
    let funded = repository.load_organization(org.org_id).await?;
    assert_eq!(funded.revision, 1);
    assert_eq!(funded.total_spent, None);
    let funded_limit = funded.limit.expect("active limit");
    assert_eq!(funded_limit.spend_limit, 10);
    assert_eq!(funded_limit.available, 10);

    let request = usage_request(
        org.org_id,
        org.workspace_a_id,
        org.api_key_a_id,
        model.id,
        model.name.clone(),
        Uuid::new_v4(),
        3,
    );
    usage.record_usage(request.clone()).await?;
    let after_post = repository.load_organization(org.org_id).await?;
    let after_post_key = repository.load_key(org.org_id, org.api_key_a_id).await?;
    assert_eq!(after_post.revision, 2);
    assert_eq!(after_post.total_spent, Some(3));
    assert_eq!(after_post.limit.expect("active limit").available, 7);
    assert_eq!(after_post_key.revision, 2);
    assert_eq!(after_post_key.inference_spent, 3);

    let service_id = Uuid::new_v4();
    pool.get()
        .await?
        .execute(
            "INSERT INTO services (id, service_name, display_name, unit, cost_per_unit) VALUES ($1, $2, 'Snapshot service', 'request', 1)",
            &[&service_id, &format!("snapshot-service-{service_id}")],
        )
        .await?;
    OrganizationServiceUsageRepository::new(pool.clone())
        .record_usage(&RecordServiceUsageRequest {
            organization_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            service_id,
            quantity: 1,
            total_cost: 1,
            inference_id: Some(Uuid::new_v4()),
        })
        .await?;
    let after_service = repository.load_organization(org.org_id).await?;
    let after_service_key = repository.load_key(org.org_id, org.api_key_a_id).await?;
    assert_eq!(after_service.revision, 3);
    assert_eq!(after_service.total_spent, Some(4));
    assert_eq!(after_service_key.revision, 3);
    assert_eq!(after_service_key.inference_spent, 3);

    usage.record_usage(request).await?;
    assert_eq!(repository.load_organization(org.org_id).await?.revision, 3);

    let mut invalid = usage_request(
        org.org_id,
        org.workspace_a_id,
        org.api_key_a_id,
        model.id,
        model.name.clone(),
        Uuid::new_v4(),
        -1,
    );
    invalid.input_cost = -1;
    assert!(usage.record_usage(invalid).await.is_err());
    assert_eq!(repository.load_organization(org.org_id).await?.revision, 3);

    api_keys
        .update_spend_limit(org.api_key_a_id, Some(9))
        .await?;
    assert_eq!(repository.load_organization(org.org_id).await?.revision, 4);
    assert_eq!(
        repository
            .load_key(org.org_id, org.api_key_a_id)
            .await?
            .spend_limit,
        Some(9)
    );

    api_keys
        .update(org.api_key_a_id, None, None, Some(None), None)
        .await?;
    assert_eq!(repository.load_organization(org.org_id).await?.revision, 5);
    assert_eq!(
        repository
            .load_key(org.org_id, org.api_key_a_id)
            .await?
            .spend_limit,
        None
    );

    assert!(repository
        .load_key(other_org.org_id, org.api_key_a_id)
        .await
        .is_err());

    cleanup_usage_fixtures(&pool, &[org.org_id, other_org.org_id], &[model.id]).await?;
    pool.get()
        .await?
        .execute("DELETE FROM services WHERE id = $1", &[&service_id])
        .await?;
    Ok(())
}

#[tokio::test]
async fn admission_spend_reads_inference_counter_and_excludes_service_counter() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let repository = PgAdmissionSnapshotRepository::new(pool.clone(), Duration::from_millis(500));

    // A valid key without a counter starts at zero; an unknown key must fail admission.
    assert_eq!(
        repository
            .load_key(org.org_id, org.api_key_a_id)
            .await?
            .inference_spent,
        0
    );
    assert!(repository
        .load_key(org.org_id, Uuid::new_v4())
        .await
        .is_err());
    let client = pool.get().await?;
    client.execute(
        "UPDATE organization_balance SET spend_counters_ready_at = NOW() WHERE organization_id = $1",
        &[&org.org_id],
    ).await?;
    for (key_id, inference_spent, service_spent) in [
        (org.api_key_a_id, 321_i64, 987_i64),
        (org.api_key_b_id, 0_i64, 654_i64),
    ] {
        client.execute(
            "INSERT INTO api_key_spend (api_key_id, inference_spent, service_spent, updated_at) VALUES ($1, $2, $3, NOW())",
            &[&key_id, &inference_spent, &service_spent],
        ).await?;
    }
    drop(client);

    assert_eq!(
        repository
            .load_key(org.org_id, org.api_key_a_id)
            .await?
            .inference_spent,
        321
    );
    assert_eq!(
        repository
            .load_key(org.org_id, org.api_key_b_id)
            .await?
            .inference_spent,
        0
    );
    cleanup_usage_fixtures(&pool, &[org.org_id], &[]).await?;
    Ok(())
}
