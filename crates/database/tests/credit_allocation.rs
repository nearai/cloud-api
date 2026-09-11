#[allow(dead_code)]
mod support;

use database::models::{RecordUsageRequest, UpdateOrganizationLimitsDbRequest};
use database::repositories::{
    credit_adjustment::{
        AdjustedUsageKind, CreateCreditAdjustment, CreditAdjustmentKind, CreditAdjustmentRepository,
    },
    OrganizationLimitsRepository, OrganizationServiceUsageRepository, OrganizationUsageRepository,
    PostgresReportingUsageSummaryRepository, RecordServiceUsageRequest,
};
use services::reporting_usage::{
    ReportingUsageSummaryFilters, ReportingUsageSummaryRepository, ReportingUsageSummarySource,
};
use services::service_usage::ports::ServiceUsageReportFilters;
use services::usage::{InferenceType, InferenceUsageReportQuery};
use std::time::Duration;
use support::{
    cleanup_usage_fixtures, insert_model, insert_org_fixture, test_pool, ModelFixture, OrgFixture,
};
use uuid::Uuid;

fn usage(
    org: &OrgFixture,
    model: &ModelFixture,
    inference_id: Uuid,
    cost: i64,
) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id: org.org_id,
        workspace_id: org.workspace_a_id,
        api_key_id: org.api_key_a_id,
        model_id: model.id,
        model_name: model.name.clone(),
        input_tokens: 1,
        output_tokens: 0,
        input_cost: cost,
        output_cost: 0,
        total_cost: cost,
        inference_type: InferenceType::ChatCompletion.as_str().to_string(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(inference_id),
        provider_request_id: Some(format!("allocation-{inference_id}")),
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

async fn set_limit(
    repository: &OrganizationLimitsRepository,
    organization_id: Uuid,
    credit_type: &str,
    amount: i64,
) -> anyhow::Result<()> {
    repository
        .update_limits(
            organization_id,
            &UpdateOrganizationLimitsDbRequest {
                spend_limit: amount,
                credit_type: credit_type.to_string(),
                source: Some(format!("test-{credit_type}")),
                currency: "USD".to_string(),
                changed_by: Some("credit-allocation-test".to_string()),
                change_reason: Some("test ceiling".to_string()),
                changed_by_user_id: None,
                changed_by_user_email: None,
            },
        )
        .await?;
    Ok(())
}

async fn set_example_limits(
    repository: &OrganizationLimitsRepository,
    organization_id: Uuid,
) -> anyhow::Result<()> {
    for (credit_type, amount) in [
        ("grant", 2),
        ("postpay", 10),
        ("staking_farm", 3),
        ("payment", 4),
    ] {
        set_limit(repository, organization_id, credit_type, amount).await?;
    }
    Ok(())
}

#[tokio::test]
async fn priority_splits_exactly_and_records_overage() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let repository = OrganizationUsageRepository::new(pool.clone());
    let model = insert_model(&pool, "allocation-priority").await?;

    for (cost, expected, unfunded) in [
        (
            12,
            vec![
                ("grant", 2),
                ("staking_farm", 3),
                ("payment", 4),
                ("postpay", 3),
            ],
            0,
        ),
        (
            17,
            vec![
                ("grant", 2),
                ("staking_farm", 3),
                ("payment", 4),
                ("postpay", 8),
            ],
            0,
        ),
        (
            20,
            vec![
                ("grant", 2),
                ("staking_farm", 3),
                ("payment", 4),
                ("postpay", 10),
            ],
            1,
        ),
    ] {
        let org = insert_org_fixture(&pool).await?;
        set_example_limits(&limits, org.org_id).await?;
        let row = repository
            .record_usage(usage(&org, &model, Uuid::new_v4(), cost))
            .await?;
        let actual = row
            .credit_allocations
            .expect("new usage must have attributed allocations")
            .into_iter()
            .map(|allocation| (allocation.credit_type, allocation.amount))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            expected
                .into_iter()
                .map(|(kind, amount)| (kind.to_string(), amount))
                .collect::<Vec<_>>()
        );
        assert_eq!(row.funded_amount, Some(cost - unfunded));
        assert_eq!(row.unfunded_amount, Some(unfunded));
        assert_eq!(row.allocation_policy_version.as_deref(), Some("v1"));
        cleanup_usage_fixtures(&pool, &[org.org_id], &[]).await?;
    }

    cleanup_usage_fixtures(&pool, &[], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn missing_disabled_exhausted_and_zero_cost_types_are_skipped() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let repository = OrganizationUsageRepository::new(pool.clone());
    let model = insert_model(&pool, "allocation-edge-cases").await?;

    let fallback_org = insert_org_fixture(&pool).await?;
    set_limit(&limits, fallback_org.org_id, "postpay", 0).await?;
    set_limit(&limits, fallback_org.org_id, "staking_farm", 3).await?;
    set_limit(&limits, fallback_org.org_id, "payment", 4).await?;
    let fallback = repository
        .record_usage(usage(&fallback_org, &model, Uuid::new_v4(), 4))
        .await?;
    assert_eq!(
        fallback
            .credit_allocations
            .unwrap()
            .into_iter()
            .map(|allocation| (allocation.credit_type, allocation.amount))
            .collect::<Vec<_>>(),
        vec![("staking_farm".to_string(), 3), ("payment".to_string(), 1)]
    );

    let postpay_org = insert_org_fixture(&pool).await?;
    set_limit(&limits, postpay_org.org_id, "postpay", 10).await?;
    let postpay = repository
        .record_usage(usage(&postpay_org, &model, Uuid::new_v4(), 10))
        .await?;
    assert_eq!(
        postpay.credit_allocations.unwrap()[0].credit_type,
        "postpay"
    );
    let zero = repository
        .record_usage(usage(&postpay_org, &model, Uuid::new_v4(), 0))
        .await?;
    assert_eq!(zero.credit_allocations, Some(Vec::new()));
    assert_eq!(zero.funded_amount, Some(0));
    assert_eq!(zero.unfunded_amount, Some(0));

    cleanup_usage_fixtures(
        &pool,
        &[fallback_org.org_id, postpay_org.org_id],
        &[model.id],
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn retry_keeps_original_split_and_conflicting_retry_is_rejected() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let repository = OrganizationUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-retry").await?;
    let other_model = insert_model(&pool, "allocation-retry-other").await?;
    set_example_limits(&limits, org.org_id).await?;
    let inference_id = Uuid::new_v4();

    let request = usage(&org, &model, inference_id, 12);
    let original = repository.record_usage(request.clone()).await?;
    set_limit(&limits, org.org_id, "grant", 100).await?;
    let retried = repository.record_usage(request.clone()).await?;

    assert!(!retried.was_inserted);
    assert_eq!(retried.id, original.id);
    assert_eq!(retried.credit_allocations, original.credit_allocations);
    let mut conflicts = Vec::new();
    let mut changed = request.clone();
    changed.workspace_id = org.workspace_b_id;
    conflicts.push(("workspace_id", changed));
    let mut changed = request.clone();
    changed.api_key_id = org.api_key_b_id;
    conflicts.push(("api_key_id", changed));
    let mut changed = request.clone();
    changed.model_id = other_model.id;
    conflicts.push(("model_id", changed));
    let mut changed = request.clone();
    changed.model_name.push_str("-different");
    conflicts.push(("model_name", changed));
    let mut changed = request.clone();
    changed.input_tokens = 2;
    conflicts.push(("input_tokens", changed));
    let mut changed = request.clone();
    changed.output_tokens = 1;
    conflicts.push(("output_tokens", changed));
    let mut changed = request.clone();
    changed.cache_read_tokens = 1;
    conflicts.push(("cache_read_tokens", changed));
    let mut changed = request.clone();
    changed.cache_write_tokens = 1;
    conflicts.push(("cache_write_tokens", changed));
    let mut changed = request.clone();
    changed.input_cost = 11;
    conflicts.push(("input_cost", changed));
    let mut changed = request.clone();
    changed.output_cost = 1;
    conflicts.push(("output_cost", changed));
    let mut changed = request.clone();
    changed.total_cost = 13;
    conflicts.push(("total_cost", changed));
    let mut changed = request.clone();
    changed.inference_type = InferenceType::Embedding.as_str().to_string();
    conflicts.push(("inference_type", changed));
    let mut changed = request.clone();
    changed.image_count = Some(1);
    conflicts.push(("image_count", changed));
    let mut changed = request.clone();
    changed.billing_details = Some(serde_json::json!({"pricing": "different"}));
    conflicts.push(("billing_details", changed));
    let mut changed = request.clone();
    changed.service_tier = Some("flex".to_string());
    conflicts.push(("service_tier", changed));
    let mut changed = request.clone();
    changed.context_band = Some("long".to_string());
    conflicts.push(("context_band", changed));

    for (field, conflicting_request) in conflicts {
        assert!(
            repository.record_usage(conflicting_request).await.is_err(),
            "a retry with conflicting {field} must be rejected"
        );
    }

    let client = pool.get().await?;
    let allocation_total: i64 = client
        .query_one(
            "SELECT COALESCE(SUM(amount), 0)::BIGINT FROM usage_credit_allocations WHERE inference_usage_id = $1",
            &[&original.id],
        )
        .await?
        .get(0);
    assert_eq!(allocation_total, 12);
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id, other_model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn funding_columns_must_be_null_or_populated_as_a_pair() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let inference_repository = OrganizationUsageRepository::new(pool.clone());
    let service_repository = OrganizationServiceUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-funding-pair").await?;
    set_limit(&limits, org.org_id, "grant", 10).await?;

    let inference = inference_repository
        .record_usage(usage(&org, &model, Uuid::new_v4(), 2))
        .await?;
    let service_id = Uuid::new_v4();
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO services (id, service_name, display_name, unit, cost_per_unit) VALUES ($1, $2, 'Funding pair service', 'request', 2)",
            &[&service_id, &format!("funding-pair-service-{service_id}")],
        )
        .await?;
    let service = service_repository
        .record_usage(&RecordServiceUsageRequest {
            organization_id: org.org_id,
            workspace_id: org.workspace_a_id,
            api_key_id: org.api_key_a_id,
            service_id,
            quantity: 1,
            total_cost: 2,
            inference_id: Some(Uuid::new_v4()),
        })
        .await?;

    for query in [
        "UPDATE organization_usage_log SET funded_amount = NULL WHERE id = $1",
        "UPDATE organization_usage_log SET unfunded_amount = NULL WHERE id = $1",
        "UPDATE organization_usage_log SET funded_amount = -1, unfunded_amount = 3 WHERE id = $1",
        "UPDATE organization_usage_log SET funded_amount = 1, unfunded_amount = 2 WHERE id = $1",
    ] {
        assert!(
            client.execute(query, &[&inference.id]).await.is_err(),
            "inference funding columns must reject a half-populated pair"
        );
    }
    for query in [
        "UPDATE organization_service_usage_log SET funded_amount = NULL WHERE id = $1",
        "UPDATE organization_service_usage_log SET unfunded_amount = NULL WHERE id = $1",
        "UPDATE organization_service_usage_log SET funded_amount = -1, unfunded_amount = 3 WHERE id = $1",
        "UPDATE organization_service_usage_log SET funded_amount = 1, unfunded_amount = 2 WHERE id = $1",
    ] {
        assert!(
            client.execute(query, &[&service.id]).await.is_err(),
            "service funding columns must reject a half-populated pair"
        );
    }

    drop(client);
    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    pool.get()
        .await?
        .execute("DELETE FROM services WHERE id = $1", &[&service_id])
        .await?;
    Ok(())
}

#[tokio::test]
async fn service_retry_preserves_allocation_and_rejects_conflicts() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let repository = OrganizationServiceUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    set_limit(&limits, org.org_id, "grant", 10).await?;
    let service_id = Uuid::new_v4();
    let other_service_id = Uuid::new_v4();
    let client = pool.get().await?;
    for (id, name) in [
        (service_id, format!("retry-service-{service_id}")),
        (
            other_service_id,
            format!("retry-service-{other_service_id}"),
        ),
    ] {
        client
            .execute(
                "INSERT INTO services (id, service_name, display_name, unit, cost_per_unit) VALUES ($1, $2, 'Retry service', 'request', 2)",
                &[&id, &name],
            )
            .await?;
    }
    drop(client);

    let request = RecordServiceUsageRequest {
        organization_id: org.org_id,
        workspace_id: org.workspace_a_id,
        api_key_id: org.api_key_a_id,
        service_id,
        quantity: 1,
        total_cost: 2,
        inference_id: Some(Uuid::new_v4()),
    };
    let original = repository.record_usage(&request).await?;
    let retried = repository.record_usage(&request).await?;
    assert_eq!(retried.id, original.id);
    assert_eq!(retried.credit_allocations, original.credit_allocations);

    let mut conflicts = Vec::new();
    let mut changed = request.clone();
    changed.workspace_id = org.workspace_b_id;
    conflicts.push(("workspace_id", changed));
    let mut changed = request.clone();
    changed.api_key_id = org.api_key_b_id;
    conflicts.push(("api_key_id", changed));
    let mut changed = request.clone();
    changed.service_id = other_service_id;
    conflicts.push(("service_id", changed));
    let mut changed = request.clone();
    changed.quantity = 2;
    conflicts.push(("quantity", changed));
    let mut changed = request.clone();
    changed.total_cost = 3;
    conflicts.push(("total_cost", changed));

    for (field, conflicting_request) in conflicts {
        assert!(
            repository.record_usage(&conflicting_request).await.is_err(),
            "a service retry with conflicting {field} must be rejected"
        );
    }

    cleanup_usage_fixtures(&pool, &[org.org_id], &[]).await?;
    pool.get()
        .await?
        .execute(
            "DELETE FROM services WHERE id IN ($1, $2)",
            &[&service_id, &other_service_id],
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn retry_preserves_unknown_attribution_for_legacy_usage() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let repository = OrganizationUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-legacy-retry").await?;
    set_limit(&limits, org.org_id, "grant", 10).await?;
    let request = usage(&org, &model, Uuid::new_v4(), 2);
    let original = repository.record_usage(request.clone()).await?;

    let client = pool.get().await?;
    client
        .execute(
            "DELETE FROM usage_credit_allocations WHERE inference_usage_id = $1",
            &[&original.id],
        )
        .await?;
    client
        .execute(
            r#"UPDATE organization_usage_log
               SET funded_amount = NULL, unfunded_amount = NULL,
                   allocation_policy_version = NULL
               WHERE id = $1"#,
            &[&original.id],
        )
        .await?;
    drop(client);

    let retried = repository.record_usage(request).await?;
    assert!(!retried.was_inserted);
    assert_eq!(retried.credit_allocations, None);
    assert_eq!(retried.funded_amount, None);
    assert_eq!(retried.unfunded_amount, None);

    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn ambiguous_legacy_spend_stays_unknown_and_cannot_restore_capacity() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let repository = OrganizationUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-legacy-baseline").await?;
    set_limit(&limits, org.org_id, "grant", 100).await?;

    let client = pool.get().await?;
    client
        .execute(
            r#"UPDATE organization_balance
               SET total_spent = 40, legacy_unattributed_amount = 40,
                   total_requests = 1, total_tokens = 1, updated_at = NOW()
               WHERE organization_id = $1"#,
            &[&org.org_id],
        )
        .await?;
    drop(client);

    let row = repository
        .record_usage(usage(&org, &model, Uuid::new_v4(), 70))
        .await?;
    assert_eq!(row.funded_amount, Some(60));
    assert_eq!(row.unfunded_amount, Some(10));
    assert_eq!(row.credit_allocations.unwrap()[0].amount, 60);

    let (status, unfunded, unattributed) = limits.get_current_credit_status(org.org_id).await?;
    assert_eq!(status[0].consumed, 60);
    assert_eq!(status[0].available, 40);
    assert_eq!(unfunded, 10);
    assert_eq!(unattributed, 40);

    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn limit_replacement_preserves_consumption_and_custom_order_is_honored() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-cumulative").await?;
    set_limit(&limits, org.org_id, "grant", 2).await?;
    set_limit(&limits, org.org_id, "postpay", 10).await?;
    let repository = OrganizationUsageRepository::new(pool.clone());
    repository
        .record_usage(usage(&org, &model, Uuid::new_v4(), 2))
        .await?;
    set_limit(&limits, org.org_id, "grant", 5).await?;
    let next = repository
        .record_usage(usage(&org, &model, Uuid::new_v4(), 4))
        .await?;
    assert_eq!(
        next.credit_allocations
            .unwrap()
            .into_iter()
            .map(|allocation| (allocation.credit_type, allocation.amount))
            .collect::<Vec<_>>(),
        vec![("grant".to_string(), 3), ("postpay".to_string(), 1)]
    );

    let custom_org = insert_org_fixture(&pool).await?;
    set_example_limits(&limits, custom_org.org_id).await?;
    let custom = config::CreditAllocationConfig {
        priority: ["payment", "staking_farm", "postpay", "grant"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        policy_version: "custom-v2".to_string(),
    };
    let custom_repository = OrganizationUsageRepository::with_accounting_config(
        pool.clone(),
        Duration::from_secs(15),
        &custom,
    );
    let custom_row = custom_repository
        .record_usage(usage(&custom_org, &model, Uuid::new_v4(), 5))
        .await?;
    assert_eq!(
        custom_row
            .credit_allocations
            .unwrap()
            .into_iter()
            .map(|allocation| (allocation.credit_type, allocation.amount))
            .collect::<Vec<_>>(),
        vec![("payment".to_string(), 4), ("staking_farm".to_string(), 1)]
    );
    assert_eq!(
        custom_row.allocation_policy_version.as_deref(),
        Some("custom-v2")
    );

    cleanup_usage_fixtures(&pool, &[org.org_id, custom_org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn corrections_reverse_last_funding_first_and_retries_are_idempotent() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let usage_repository = OrganizationUsageRepository::new(pool.clone());
    let adjustment_repository = CreditAdjustmentRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-correction").await?;
    set_example_limits(&limits, org.org_id).await?;
    let usage_request = usage(&org, &model, Uuid::new_v4(), 17);
    let usage = usage_repository.record_usage(usage_request.clone()).await?;
    let request = CreateCreditAdjustment {
        organization_id: org.org_id,
        usage_id: usage.id,
        usage_kind: AdjustedUsageKind::Inference,
        adjustment_kind: CreditAdjustmentKind::Correction,
        amount: 4,
        reason: "billing correction".to_string(),
        idempotency_key: Uuid::new_v4().to_string(),
        changed_by_user_id: None,
        changed_by_user_email: Some("billing@example.test".to_string()),
    };

    let correction = adjustment_repository.create(&request).await?;
    assert_eq!(correction.unfunded_amount_reversed, 0);
    assert_eq!(
        correction
            .allocation_reversals
            .iter()
            .map(|reversal| (reversal.credit_type.as_str(), reversal.amount))
            .collect::<Vec<_>>(),
        vec![("postpay", 4)]
    );
    let retried = adjustment_repository.create(&request).await?;
    assert_eq!(retried.id, correction.id);
    let mut conflicting = request.clone();
    conflicting.amount = 3;
    assert!(adjustment_repository.create(&conflicting).await.is_err());

    let (status, unfunded, _) = limits.get_current_credit_status(org.org_id).await?;
    let consumed = status
        .iter()
        .map(|status| (status.limit.credit_type.as_str(), status.consumed))
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(consumed["grant"], 2);
    assert_eq!(consumed["postpay"], 4);
    assert_eq!(consumed["staking_farm"], 3);
    assert_eq!(consumed["payment"], 4);
    assert_eq!(unfunded, 0);
    assert_eq!(
        usage_repository
            .get_balance(org.org_id)
            .await?
            .unwrap()
            .total_spent,
        13
    );
    assert_eq!(
        usage_repository.get_api_key_spend(org.api_key_a_id).await?,
        13
    );

    let retried_usage = usage_repository.record_usage(usage_request).await?;
    assert_eq!(retried_usage.total_cost, 13);
    assert_eq!(retried_usage.funded_amount, Some(13));
    assert_eq!(retried_usage.unfunded_amount, Some(0));
    assert_eq!(
        retried_usage
            .credit_allocations
            .unwrap()
            .into_iter()
            .map(|allocation| (allocation.credit_type, allocation.amount))
            .collect::<Vec<_>>(),
        vec![
            ("grant".to_string(), 2),
            ("staking_farm".to_string(), 3),
            ("payment".to_string(), 4),
            ("postpay".to_string(), 4),
        ]
    );

    let history = usage_repository
        .get_usage_history(org.org_id, Some(10), Some(0))
        .await?;
    let adjusted_history = history
        .iter()
        .find(|row| row.id == usage.id)
        .expect("corrected usage should remain in history");
    assert_eq!(adjusted_history.total_cost, 13);
    assert_eq!(adjusted_history.funded_amount, Some(13));
    assert_eq!(adjusted_history.unfunded_amount, Some(0));

    let all_rows = usage_repository
        .list_inference_usage_report(InferenceUsageReportQuery::for_organization(org.org_id))
        .await?;
    assert_eq!(all_rows[0].total_cost_nano_usd, 13);
    let payment_rows = usage_repository
        .list_inference_usage_report(InferenceUsageReportQuery {
            credit_type: Some("payment".to_string()),
            ..InferenceUsageReportQuery::for_organization(org.org_id)
        })
        .await?;
    assert_eq!(payment_rows[0].total_cost_nano_usd, 4);
    let staking_rows = usage_repository
        .list_inference_usage_report(InferenceUsageReportQuery {
            credit_type: Some("staking_farm".to_string()),
            ..InferenceUsageReportQuery::for_organization(org.org_id)
        })
        .await?;
    assert_eq!(staking_rows[0].total_cost_nano_usd, 3);

    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn writeoff_clears_only_unfunded_debt_and_later_capacity_remains_separate(
) -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let usage_repository = OrganizationUsageRepository::new(pool.clone());
    let adjustment_repository = CreditAdjustmentRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-writeoff").await?;
    set_example_limits(&limits, org.org_id).await?;
    let usage = usage_repository
        .record_usage(usage(&org, &model, Uuid::new_v4(), 20))
        .await?;
    set_limit(&limits, org.org_id, "payment", 5).await?;
    assert_eq!(limits.get_current_credit_status(org.org_id).await?.1, 1);

    let writeoff = adjustment_repository
        .create(&CreateCreditAdjustment {
            organization_id: org.org_id,
            usage_id: usage.id,
            usage_kind: AdjustedUsageKind::Inference,
            adjustment_kind: CreditAdjustmentKind::Writeoff,
            amount: 1,
            reason: "approved write-off".to_string(),
            idempotency_key: Uuid::new_v4().to_string(),
            changed_by_user_id: None,
            changed_by_user_email: None,
        })
        .await?;
    assert_eq!(writeoff.unfunded_amount_reversed, 1);
    assert!(writeoff.allocation_reversals.is_empty());
    let (status, unfunded, _) = limits.get_current_credit_status(org.org_id).await?;
    assert_eq!(unfunded, 0);
    assert_eq!(
        status
            .iter()
            .find(|status| status.limit.credit_type == "payment")
            .unwrap()
            .available,
        1
    );

    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_inference_and_service_cannot_double_spend_capacity() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let limits = OrganizationLimitsRepository::new(pool.clone());
    let inference_repository = OrganizationUsageRepository::new(pool.clone());
    let service_repository = OrganizationServiceUsageRepository::new(pool.clone());
    let org = insert_org_fixture(&pool).await?;
    let model = insert_model(&pool, "allocation-concurrency").await?;
    set_limit(&limits, org.org_id, "grant", 10).await?;
    let service_id = Uuid::new_v4();
    pool.get()
        .await?
        .execute(
            "INSERT INTO services (id, service_name, display_name, unit, cost_per_unit) VALUES ($1, $2, 'Allocation service', 'request', 8)",
            &[&service_id, &format!("allocation-service-{service_id}")],
        )
        .await?;

    let inference_id = Uuid::new_v4();
    let service_inference_id = Uuid::new_v4();
    let inference = inference_repository.record_usage(usage(&org, &model, inference_id, 8));
    let service_request = RecordServiceUsageRequest {
        organization_id: org.org_id,
        workspace_id: org.workspace_a_id,
        api_key_id: org.api_key_a_id,
        service_id,
        quantity: 1,
        total_cost: 8,
        inference_id: Some(service_inference_id),
    };
    let service = service_repository.record_usage(&service_request);
    let (inference, service) = tokio::try_join!(inference, service)?;

    assert_eq!(
        inference.funded_amount.unwrap() + service.funded_amount.unwrap(),
        10
    );
    assert_eq!(
        inference.unfunded_amount.unwrap() + service.unfunded_amount.unwrap(),
        6
    );
    let allocated: i64 = pool
        .get()
        .await?
        .query_one(
            "SELECT COALESCE(SUM(amount), 0)::BIGINT FROM usage_credit_allocations WHERE organization_id = $1 AND credit_type = 'grant'",
            &[&org.org_id],
        )
        .await?
        .get(0);
    assert_eq!(allocated, 10);

    // Credit filtering keeps each mixed charge once and substitutes only the
    // matching allocation amount, so request/token counts cannot inflate.
    let inference_rows = inference_repository
        .list_inference_usage_report(InferenceUsageReportQuery {
            credit_type: Some("grant".to_string()),
            ..InferenceUsageReportQuery::for_organization(org.org_id)
        })
        .await?;
    let service_rows = service_repository
        .list_reporting_usage(&ServiceUsageReportFilters {
            organization_id: org.org_id,
            credit_type: Some("grant".to_string()),
            limit: 10,
            ..ServiceUsageReportFilters::default()
        })
        .await?;
    assert_eq!(inference_rows.len(), 1);
    assert_eq!(service_rows.len(), 1);
    assert_eq!(
        inference_rows[0].total_cost_nano_usd + service_rows[0].total_cost,
        10
    );
    assert_eq!(
        inference_rows[0].credit_allocations.as_ref().unwrap().len(),
        1
    );
    assert_eq!(
        service_rows[0].credit_allocations.as_ref().unwrap().len(),
        1
    );

    let summary = PostgresReportingUsageSummaryRepository::new(pool.clone())
        .summarize_usage(&ReportingUsageSummaryFilters {
            organization_id: org.org_id,
            start_time: None,
            end_time: None,
            workspace_id: None,
            api_key_id: None,
            model: None,
            inference_type: None,
            service_name: None,
            credit_type: Some("grant".to_string()),
            source: ReportingUsageSummarySource::All,
            deadline: None,
        })
        .await?;
    assert_eq!(
        summary.inference.totals.total_cost_nano_usd + summary.service.totals.total_cost_nano_usd,
        10
    );
    assert_eq!(summary.inference.totals.request_count, 1);
    assert_eq!(summary.service.totals.usage_count, 1);

    cleanup_usage_fixtures(&pool, &[org.org_id], &[model.id]).await?;
    pool.get()
        .await?
        .execute("DELETE FROM services WHERE id = $1", &[&service_id])
        .await?;
    Ok(())
}
