//! Exercise typed billing parameters against PostgreSQL, including populated
//! optional values, duplicate writes, and rollback after a partial write.

#[allow(dead_code)]
mod support;

use database::models::{RecordUsageRequest, ServedProviderTier, ServedProviderType, StopReason};
use database::repositories::{ModelRepository, OrganizationUsageRepository};
use support::{cleanup_usage_fixtures, insert_model, insert_org_fixture, test_pool};
use uuid::Uuid;

fn request(org: &support::OrgFixture, model: &support::ModelFixture) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id: org.org_id,
        workspace_id: org.workspace_a_id,
        api_key_id: org.api_key_a_id,
        model_id: model.id,
        model_name: model.name.clone(),
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_write_tokens: 3,
        input_cost: 100,
        output_cost: 200,
        total_cost: 300,
        inference_type: "chat_completion".into(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: None,
        provider_request_id: None,
        stop_reason: None,
        response_id: None,
        image_count: None,
        billing_details: None,
        service_tier: None,
        context_band: None,
        served_provider_tier: None,
        served_provider_type: None,
        served_via_fallback: false,
    }
}

#[tokio::test]
async fn typed_billing_preserves_optional_fields_and_duplicate_accounting() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "typed-billing").await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    let response_id = Uuid::new_v4();
    pool.get().await?.execute(
        "INSERT INTO responses (id, model, status, workspace_id, api_key_id) VALUES ($1, $2, 'completed', $3, $4)",
        &[&response_id, &model.name, &org.workspace_a_id, &org.api_key_a_id],
    ).await?;

    for populated in [false, true] {
        let mut request = request(&org, &model);
        if populated {
            request.ttft_ms = Some(123);
            request.avg_itl_ms = Some(4.5);
            request.inference_id = Some(Uuid::new_v4());
            request.provider_request_id = Some("synthetic-provider-request".into());
            request.stop_reason = Some(StopReason::Completed);
            request.response_id = Some(response_id.into());
            request.image_count = Some(2);
            request.billing_details = Some(serde_json::json!({"test": "typed-billing"}));
            request.service_tier = Some("priority".into());
            request.context_band = Some("long".into());
            request.served_provider_tier = Some(ServedProviderTier::Attested3p);
            request.served_provider_type = Some(ServedProviderType::External);
            request.served_via_fallback = true;
        }
        let stored = repository.record_usage(request.clone()).await?;
        assert!(stored.was_inserted);
        assert_eq!(stored.total_cost, request.total_cost);
        assert_eq!(stored.total_tokens, 15);
        assert_eq!(stored.cache_read_tokens, request.cache_read_tokens);
        assert_eq!(stored.cache_write_tokens, request.cache_write_tokens);
        assert_eq!(stored.ttft_ms, request.ttft_ms);
        assert_eq!(stored.avg_itl_ms, request.avg_itl_ms);
        assert_eq!(stored.inference_id, request.inference_id);
        assert_eq!(stored.provider_request_id, request.provider_request_id);
        assert_eq!(stored.stop_reason, request.stop_reason);
        assert_eq!(
            stored.response_id.as_ref().map(|id| id.as_uuid()),
            request.response_id.as_ref().map(|id| id.as_uuid())
        );
        assert_eq!(stored.image_count, request.image_count);
        assert_eq!(stored.billing_details, request.billing_details);
        assert_eq!(stored.service_tier, request.service_tier);
        assert_eq!(stored.context_band, request.context_band);
        assert_eq!(stored.served_provider_tier, request.served_provider_tier);
        assert_eq!(stored.served_provider_type, request.served_provider_type);
        assert_eq!(stored.served_via_fallback, request.served_via_fallback);
        assert_eq!(stored.funded_amount, Some(0));
        assert_eq!(stored.unfunded_amount, Some(300));
        assert!(stored.allocation_policy_version.is_some());
        if populated {
            let duplicate = repository.record_usage(request).await?;
            assert!(!duplicate.was_inserted);
            assert_eq!(duplicate.id, stored.id);
            assert_eq!(duplicate.credit_allocations, stored.credit_allocations);
        }
    }
    let balance = repository.get_balance(org.org_id).await?.unwrap();
    assert_eq!(balance.total_spent, 600);
    assert_eq!(balance.total_requests, 2);
    assert_eq!(balance.total_tokens, 30);
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn typed_balance_failure_rolls_back_usage_and_allocation_before_retry() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "typed-billing-rollback").await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    let mut request = request(&org, &model);
    request.inference_id = Some(Uuid::new_v4());
    // Overflow only the final balance upsert, after the usage INSERT and
    // funding UPDATE have succeeded. No shared schema changes are needed.
    pool.get()
        .await?
        .execute(
            "UPDATE organization_balance SET total_spent = $2 WHERE organization_id = $1",
            &[&org.org_id, &i64::MAX],
        )
        .await?;
    let error = repository.record_usage(request.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains("22003"), "{error:#}");
    let client = pool.get().await?;
    let row = client.query_one(
        "SELECT (SELECT COUNT(*) FROM organization_usage_log WHERE organization_id = $1) AS usage_count,
                (SELECT COUNT(*) FROM usage_credit_allocations WHERE organization_id = $1) AS allocation_count,
                unresolved_unfunded_amount FROM organization_balance WHERE organization_id = $1",
        &[&org.org_id],
    ).await?;
    assert_eq!(row.get::<_, i64>("usage_count"), 0);
    assert_eq!(row.get::<_, i64>("allocation_count"), 0);
    assert_eq!(row.get::<_, i64>("unresolved_unfunded_amount"), 0);
    client
        .execute(
            "UPDATE organization_balance SET total_spent = 0 WHERE organization_id = $1",
            &[&org.org_id],
        )
        .await?;
    drop(client);
    let stored = repository.record_usage(request.clone()).await?;
    assert!(stored.was_inserted);
    let duplicate = repository.record_usage(request).await?;
    assert!(!duplicate.was_inserted);
    assert_eq!(duplicate.id, stored.id);
    let balance = repository.get_balance(org.org_id).await?.unwrap();
    assert_eq!(balance.total_spent, 300);
    assert_eq!(balance.total_requests, 1);
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn typed_model_lookup_reads_current_prices_even_after_deactivation() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let model = insert_model(&pool, "typed-billing-model").await?;
    let repository = ModelRepository::new(pool.clone());
    assert!(repository.get_by_id(&Uuid::new_v4()).await?.is_none());
    assert_eq!(
        repository
            .get_by_id(&model.id)
            .await?
            .unwrap()
            .input_cost_per_token,
        10
    );
    pool.get()
        .await?
        .execute(
            "UPDATE models SET is_active = false, input_cost_per_token = 99 WHERE id = $1",
            &[&model.id],
        )
        .await?;
    for _ in 0..2 {
        let current = repository.get_by_id(&model.id).await?.unwrap();
        assert!(!current.is_active);
        assert_eq!(current.input_cost_per_token, 99);
    }
    cleanup_usage_fixtures(&pool, &[], &[model.id]).await?;
    Ok(())
}
