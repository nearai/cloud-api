use crate::common::{
    create_api_key_in_workspace, create_org, list_workspaces, setup_test_server_with_database,
    E2E_QWEN_MODEL_NAME,
};
use database::models::RecordUsageRequest;
use database::repositories::{
    OrganizationServiceUsageRepository, OrganizationUsageRepository, RecordServiceUsageRequest,
};
use services::usage::InferenceType;
use uuid::Uuid;

async fn fixture() -> (
    axum_test::TestServer,
    std::sync::Arc<database::Database>,
    RecordUsageRequest,
    Uuid,
) {
    let (server, database) = setup_test_server_with_database().await;
    let org = create_org(&server).await;
    let workspace = list_workspaces(&server, org.id.clone())
        .await
        .into_iter()
        .next()
        .expect("organization has a default workspace");
    let api_key = create_api_key_in_workspace(
        &server,
        workspace.id.clone(),
        "spend-counter-key".to_string(),
    )
    .await;
    let client = database.pool().get().await.expect("database connection");
    let model = client
        .query_one(
            "SELECT id, model_name FROM models WHERE model_name = $1",
            &[&E2E_QWEN_MODEL_NAME],
        )
        .await
        .expect("shared model fixture");
    let model_id: Uuid = model.get("id");
    let model_name: String = model.get("model_name");
    drop(client);

    let request = RecordUsageRequest {
        organization_id: org.id.parse().expect("organization UUID"),
        workspace_id: workspace.id.parse().expect("workspace UUID"),
        api_key_id: api_key.id.parse().expect("API key UUID"),
        model_id,
        model_name,
        input_tokens: 1,
        output_tokens: 1,
        input_cost: 1_000,
        output_cost: 2_000,
        total_cost: 3_000,
        inference_type: InferenceType::ChatCompletion.as_str().to_string(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: Some("spend-counter-test".to_string()),
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
    };
    (
        server,
        database,
        request,
        api_key.id.parse().expect("API key UUID"),
    )
}

#[tokio::test]
async fn spend_counters_track_inference_and_service_separately_on_one_key() -> anyhow::Result<()> {
    let (_server, database, inference, api_key_id) = fixture().await;
    let inference_repository = OrganizationUsageRepository::new(database.pool().clone());
    let service_repository = OrganizationServiceUsageRepository::new(database.pool().clone());
    let service_id = Uuid::new_v4();
    let client = database.pool().get().await?;
    client
        .execute(
            "INSERT INTO services (id, service_name, display_name, unit, cost_per_unit) VALUES ($1, $2, $2, 'request', 5000)",
            &[&service_id, &format!("spend-counter-service-{service_id}")],
        )
        .await?;
    drop(client);

    let service = RecordServiceUsageRequest {
        organization_id: inference.organization_id,
        workspace_id: inference.workspace_id,
        api_key_id,
        service_id,
        quantity: 1,
        total_cost: 5_000,
        inference_id: Some(Uuid::new_v4()),
    };
    service_repository.record_usage(&service).await?;
    service_repository.record_usage(&service).await?;
    let mut second_service = service.clone();
    second_service.inference_id = Some(Uuid::new_v4());
    second_service.total_cost = 2_000;
    service_repository.record_usage(&second_service).await?;
    inference_repository.record_usage(inference.clone()).await?;
    let mut second_inference = inference.clone();
    second_inference.inference_id = Some(Uuid::new_v4());
    second_inference.provider_request_id = Some("spend-counter-second-inference".to_string());
    second_inference.total_cost = 1_500;
    second_inference.input_cost = 1_500;
    second_inference.output_cost = 0;
    inference_repository.record_usage(second_inference).await?;

    let client = database.pool().get().await?;
    let counters = client
        .query_one(
            "SELECT inference_spent, service_spent FROM api_key_spend WHERE api_key_id = $1",
            &[&api_key_id],
        )
        .await?;
    assert_eq!(counters.get::<_, i64>("inference_spent"), 4_500);
    assert_eq!(counters.get::<_, i64>("service_spent"), 7_000);
    let balance = client
        .query_one(
            "SELECT inference_spent, service_spent FROM organization_balance WHERE organization_id = $1",
            &[&inference.organization_id],
        )
        .await?;
    assert_eq!(balance.get::<_, i64>("inference_spent"), 4_500);
    assert_eq!(balance.get::<_, i64>("service_spent"), 7_000);
    Ok(())
}

#[tokio::test]
async fn spend_counters_ignore_duplicates_and_failed_posts() -> anyhow::Result<()> {
    let (_server, database, inference, api_key_id) = fixture().await;
    let repository = OrganizationUsageRepository::new(database.pool().clone());
    repository.record_usage(inference.clone()).await?;
    repository.record_usage(inference.clone()).await?;

    let client = database.pool().get().await?;
    client
        .execute(
            "UPDATE api_key_spend SET inference_spent = $2 WHERE api_key_id = $1",
            &[&api_key_id, &i64::MAX],
        )
        .await?;
    drop(client);

    let overflow_id = Uuid::new_v4();
    let mut overflow = inference.clone();
    overflow.inference_id = Some(overflow_id);
    overflow.provider_request_id = Some("spend-counter-overflow".to_string());
    overflow.total_cost = 1;
    overflow.input_cost = 1;
    overflow.output_cost = 0;
    let error = repository
        .record_usage(overflow)
        .await
        .expect_err("a counter overflow must roll back the usage transaction");
    let services::common::RepositoryError::DatabaseError(cause) = error
        .downcast::<services::common::RepositoryError>()
        .expect("repository error")
    else {
        panic!("expected a database overflow error");
    };
    assert_eq!(
        cause.to_string(),
        "Database error (22003): bigint out of range"
    );

    let client = database.pool().get().await?;
    let key = client
        .query_one(
            "SELECT inference_spent, service_spent FROM api_key_spend WHERE api_key_id = $1",
            &[&api_key_id],
        )
        .await?;
    assert_eq!(key.get::<_, i64>("inference_spent"), i64::MAX);
    assert_eq!(key.get::<_, i64>("service_spent"), 0);
    let usage_count: i64 = client
        .query_one(
            "SELECT COUNT(*)::BIGINT FROM organization_usage_log WHERE inference_id = $1",
            &[&inference.inference_id],
        )
        .await?
        .get(0);
    assert_eq!(usage_count, 1);
    let overflow_count: i64 = client
        .query_one(
            "SELECT COUNT(*)::BIGINT FROM organization_usage_log WHERE inference_id = $1",
            &[&overflow_id],
        )
        .await?
        .get(0);
    assert_eq!(overflow_count, 0);
    let balance = client
        .query_one(
            "SELECT total_spent, inference_spent, service_spent, unresolved_unfunded_amount FROM organization_balance WHERE organization_id = $1",
            &[&inference.organization_id],
        )
        .await?;
    assert_eq!(balance.get::<_, i64>("total_spent"), 3_000);
    assert_eq!(balance.get::<_, i64>("inference_spent"), 3_000);
    assert_eq!(balance.get::<_, i64>("service_spent"), 0);
    assert_eq!(balance.get::<_, i64>("unresolved_unfunded_amount"), 3_000);
    client
        .execute(
            "UPDATE api_key_spend SET inference_spent = $2 WHERE api_key_id = $1",
            &[&api_key_id, &3_000i64],
        )
        .await?;
    Ok(())
}
