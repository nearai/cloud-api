// E2E tests for /v1/admin/models/pricing-changes (scheduled pricing changes)

use crate::common::*;
use api::models::BatchUpdateModelApiRequest;
use std::sync::Arc;

fn minimal_model_upsert() -> serde_json::Value {
    serde_json::json!({
        "inputCostPerToken":  { "amount": 1_000, "currency": "USD" },
        "outputCostPerToken": { "amount": 2_000, "currency": "USD" },
        "modelDisplayName":   "Pricing Change Test Model",
        "modelDescription":   "Synthetic model for pricing change e2e",
        "contextLength":      4096,
        "verifiable":         false,
        "isActive":           true,
    })
}

async fn create_model(server: &axum_test::TestServer, name: &str) {
    let mut batch = BatchUpdateModelApiRequest::new();
    batch.insert(
        name.to_string(),
        serde_json::from_value(minimal_model_upsert()).unwrap(),
    );
    let updated = admin_batch_upsert_models(server, batch, get_session_id()).await;
    assert_eq!(updated.len(), 1, "Model should be created");
}

fn change_item(model: &str, effective_at: &str, input_amount: i64) -> serde_json::Value {
    serde_json::json!({
        "modelId": model,
        "effectiveAt": effective_at,
        "inputCostPerToken": { "amount": input_amount, "currency": "USD" },
    })
}

async fn post_pricing_changes(
    server: &axum_test::TestServer,
    action: &str,
    body: serde_json::Value,
) -> axum_test::TestResponse {
    server
        .post(&format!("/v1/admin/models/pricing-changes/{action}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&body)
        .await
}

async fn list_pricing_changes(server: &axum_test::TestServer, query: &str) -> serde_json::Value {
    let resp = server
        .get(&format!("/v1/admin/models/pricing-changes{query}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(resp.status_code(), 200, "list failed: {}", resp.text());
    serde_json::from_str(&resp.text()).unwrap()
}

/// Minimal `ModelsServiceTrait` stub for driving the scheduler in tests;
/// only `invalidate_models_cache` is exercised by `run_once`.
struct NoopModelsService;

#[async_trait::async_trait]
impl services::models::ModelsServiceTrait for NoopModelsService {
    async fn get_models(
        &self,
    ) -> Result<Vec<services::models::ModelInfo>, services::models::ModelsError> {
        unimplemented!()
    }
    async fn get_models_with_pricing(
        &self,
    ) -> Result<Vec<services::models::ModelWithPricing>, services::models::ModelsError> {
        unimplemented!()
    }
    async fn get_model_by_name(
        &self,
        _model_name: &str,
    ) -> Result<services::models::ModelWithPricing, services::models::ModelsError> {
        unimplemented!()
    }
    async fn resolve_and_get_model(
        &self,
        _identifier: &str,
    ) -> Result<services::models::ModelWithPricing, services::models::ModelsError> {
        unimplemented!()
    }
    async fn resolve_alias_cached(&self, _identifier: &str) -> Option<String> {
        None
    }
    async fn get_configured_model_names(
        &self,
    ) -> Result<Vec<String>, services::models::ModelsError> {
        unimplemented!()
    }
    async fn invalidate_models_cache(&self) {}
}

fn make_scheduler(database: &Arc<database::Database>) -> services::admin::ModelPricingScheduler {
    services::admin::ModelPricingScheduler::new(
        Arc::new(database::repositories::AdminCompositeRepository::new(
            database.pool().clone(),
        )),
        Arc::new(NoopModelsService),
    )
}

/// Backdate all pending changes of a batch so the scheduler considers them due.
async fn backdate_batch(database: &Arc<database::Database>, batch_id: &str) {
    let client = database.pool().get().await.unwrap();
    let batch_uuid = uuid::Uuid::parse_str(batch_id).unwrap();
    client
        .execute(
            "UPDATE scheduled_model_pricing_changes
             SET effective_at = NOW() - interval '1 minute'
             WHERE batch_id = $1",
            &[&batch_uuid],
        )
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_confirm_schedules_batch_and_lists_pending() {
    let server = setup_test_server().await;
    let model_a = format!("pricing-change-a-{}", uuid::Uuid::new_v4());
    let model_b = format!("pricing-change-b-{}", uuid::Uuid::new_v4());
    create_model(&server, &model_a).await;
    create_model(&server, &model_b).await;

    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({
            "changes": [
                change_item(&model_a, "2030-01-01T00:00:00Z", 1_500),
                change_item(&model_b, "2030-02-01", 3_000),
            ],
            "changeReason": "e2e schedule test"
        }),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "confirm failed: {}", resp.text());
    let body: serde_json::Value = serde_json::from_str(&resp.text()).unwrap();
    assert_eq!(body["changes"].as_array().unwrap().len(), 2);
    for change in body["changes"].as_array().unwrap() {
        assert_eq!(change["status"], "pending");
        // Snapshot of the pricing at confirm time.
        assert_eq!(change["oldPricing"]["inputCostPerToken"]["amount"], 1_000);
    }

    let listed = list_pricing_changes(&server, "?status=pending&limit=500").await;
    let listed_models: Vec<&str> = listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["modelId"].as_str().unwrap())
        .collect();
    assert!(listed_models.contains(&model_a.as_str()));
    assert!(listed_models.contains(&model_b.as_str()));

    let a = listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["modelId"] == model_a.as_str())
        .unwrap();
    assert_eq!(a["newPricing"]["inputCostPerToken"]["amount"], 1_500);
    assert!(
        a["newPricing"]["outputCostPerToken"].is_null(),
        "untouched fields must be omitted from newPricing"
    );
    assert_eq!(a["effectiveAt"], "2030-01-01T00:00:00Z");
}

#[tokio::test]
async fn test_validation_errors() {
    let server = setup_test_server().await;
    let model = format!("pricing-change-validate-{}", uuid::Uuid::new_v4());
    create_model(&server, &model).await;

    // effectiveAt in the past (lead time not met)
    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [change_item(&model, "2020-01-01", 1_500)] }),
    )
    .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    assert!(resp.text().contains("effectiveAt"));

    // Malformed effectiveAt (off-hour instant)
    let resp = post_pricing_changes(
        &server,
        "preview",
        serde_json::json!({ "changes": [change_item(&model, "2030-01-01T00:30:00Z", 1_500)] }),
    )
    .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());

    // No pricing fields at all
    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [
            { "modelId": model, "effectiveAt": "2030-01-01" }
        ] }),
    )
    .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    assert!(resp.text().contains("at least one pricing field"));

    // Negative amount
    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [change_item(&model, "2030-01-01", -5)] }),
    )
    .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());

    // Duplicate model in one batch
    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [
            change_item(&model, "2030-01-01", 1_500),
            change_item(&model, "2030-02-01", 1_600),
        ] }),
    )
    .await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
    assert!(resp.text().contains("more than once"));

    // Unknown model
    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [
            change_item(&format!("missing-{}", uuid::Uuid::new_v4()), "2030-01-01", 1_500)
        ] }),
    )
    .await;
    assert_eq!(resp.status_code(), 404, "{}", resp.text());

    // Empty batch
    let resp = post_pricing_changes(&server, "confirm", serde_json::json!({ "changes": [] })).await;
    assert_eq!(resp.status_code(), 400, "{}", resp.text());
}

#[tokio::test]
async fn test_conflict_and_idempotent_retry() {
    let server = setup_test_server().await;
    let model = format!("pricing-change-conflict-{}", uuid::Uuid::new_v4());
    create_model(&server, &model).await;

    let batch_id = uuid::Uuid::new_v4();
    let body = serde_json::json!({
        "batchId": batch_id,
        "changes": [change_item(&model, "2030-01-01", 1_500)],
    });

    let first = post_pricing_changes(&server, "confirm", body.clone()).await;
    assert_eq!(first.status_code(), 200, "{}", first.text());
    let first_body: serde_json::Value = serde_json::from_str(&first.text()).unwrap();
    let change_id = first_body["changes"][0]["id"].as_str().unwrap().to_string();

    // A different batch targeting the same model conflicts with the open change.
    let other = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [change_item(&model, "2030-03-01", 1_800)] }),
    )
    .await;
    assert_eq!(other.status_code(), 409, "{}", other.text());

    // Retrying the SAME batch is idempotent: same schedule row, no duplicates.
    let retry = post_pricing_changes(&server, "confirm", body).await;
    assert_eq!(retry.status_code(), 200, "{}", retry.text());
    let retry_body: serde_json::Value = serde_json::from_str(&retry.text()).unwrap();
    assert_eq!(retry_body["changes"].as_array().unwrap().len(), 1);
    assert_eq!(retry_body["changes"][0]["id"].as_str().unwrap(), change_id);
}

#[tokio::test]
async fn test_concurrent_same_batch_confirms_are_idempotent() {
    let server = setup_test_server().await;
    let model = format!("pricing-change-race-batch-{}", uuid::Uuid::new_v4());
    create_model(&server, &model).await;

    let body = serde_json::json!({
        "batchId": uuid::Uuid::new_v4(),
        "changes": [change_item(&model, "2030-01-01", 1_500)],
    });

    // A network-retry race: the same confirm fired twice concurrently must
    // not surface a 409 from the open-change index — both requests resolve
    // to the same scheduled row (advisory lock serializes them).
    let (a, b) = tokio::join!(
        post_pricing_changes(&server, "confirm", body.clone()),
        post_pricing_changes(&server, "confirm", body.clone()),
    );
    assert_eq!(a.status_code(), 200, "{}", a.text());
    assert_eq!(b.status_code(), 200, "{}", b.text());

    let a: serde_json::Value = serde_json::from_str(&a.text()).unwrap();
    let b: serde_json::Value = serde_json::from_str(&b.text()).unwrap();
    assert_eq!(a["changes"].as_array().unwrap().len(), 1);
    assert_eq!(
        a["changes"][0]["id"].as_str().unwrap(),
        b["changes"][0]["id"].as_str().unwrap(),
        "both confirms must resolve to the same scheduled row"
    );
}

// ---------------------------------------------------------------------------
// Cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cancel_pending_change() {
    let server = setup_test_server().await;
    let model = format!("pricing-change-cancel-{}", uuid::Uuid::new_v4());
    create_model(&server, &model).await;

    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [change_item(&model, "2030-01-01", 1_500)] }),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = serde_json::from_str(&resp.text()).unwrap();
    let change_id = body["changes"][0]["id"].as_str().unwrap().to_string();

    let cancel = server
        .delete(&format!("/v1/admin/models/pricing-changes/{change_id}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(cancel.status_code(), 200, "{}", cancel.text());
    let cancelled: serde_json::Value = serde_json::from_str(&cancel.text()).unwrap();
    assert_eq!(cancelled["status"], "cancelled");

    // Cancelling again: no longer pending -> 404.
    let again = server
        .delete(&format!("/v1/admin/models/pricing-changes/{change_id}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(again.status_code(), 404, "{}", again.text());

    // The model can be rescheduled after the cancel (no open-change conflict).
    let reschedule = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": [change_item(&model, "2030-02-01", 1_600)] }),
    )
    .await;
    assert_eq!(reschedule.status_code(), 200, "{}", reschedule.text());
}

// ---------------------------------------------------------------------------
// Auto-apply
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_scheduler_applies_due_changes() {
    let (server, database) = setup_test_server_with_database().await;
    let model = format!("pricing-change-apply-{}", uuid::Uuid::new_v4());
    create_model(&server, &model).await;

    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({
            "changes": [serde_json::json!({
                "modelId": model,
                "effectiveAt": "2030-01-01T00:00:00Z",
                "inputCostPerToken":  { "amount": 1_500, "currency": "USD" },
                "outputCostPerToken": { "amount": 4_000, "currency": "USD" },
            })],
            "changeReason": "e2e apply test"
        }),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = serde_json::from_str(&resp.text()).unwrap();
    let batch_id = body["batchId"].as_str().unwrap().to_string();

    // Not due yet: a scheduler pass must not touch it.
    let scheduler = make_scheduler(&database);
    scheduler.run_once().await.unwrap();
    let listed = list_pricing_changes(&server, "?status=pending&limit=500").await;
    assert!(listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["modelId"] == model.as_str()));

    // Make it due and run a pass.
    backdate_batch(&database, &batch_id).await;
    scheduler.run_once().await.unwrap();

    // The schedule row is applied...
    let listed = list_pricing_changes(&server, "?status=applied&limit=500").await;
    let applied = listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["modelId"] == model.as_str())
        .expect("change should be applied");
    assert!(applied["appliedAt"].is_string());

    // ...the live model pricing switched...
    let models_resp = server
        .get("/v1/admin/models?limit=500&include_inactive=true")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    let models: serde_json::Value = serde_json::from_str(&models_resp.text()).unwrap();
    let updated = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["modelId"] == model.as_str())
        .expect("model should exist");
    assert_eq!(updated["inputCostPerToken"]["amount"], 1_500);
    assert_eq!(updated["outputCostPerToken"]["amount"], 4_000);

    // ...and model history records the batch in its change reason.
    let history_resp = server
        .get(&format!("/v1/admin/models/{model}/history"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    let history: serde_json::Value = serde_json::from_str(&history_resp.text()).unwrap();
    assert!(
        history["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["changeReason"]
                .as_str()
                .is_some_and(|r| r.contains(&batch_id))),
        "history should record the scheduled change: {history}"
    );

    // An applied change can no longer be cancelled.
    let change_id = applied["id"].as_str().unwrap();
    let cancel = server
        .delete(&format!("/v1/admin/models/pricing-changes/{change_id}"))
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .await;
    assert_eq!(cancel.status_code(), 404, "{}", cancel.text());
}

#[tokio::test]
async fn test_concurrent_schedulers_apply_each_change_once() {
    let (server, database) = setup_test_server_with_database().await;

    let models: Vec<String> = (0..5)
        .map(|i| format!("pricing-change-race-{i}-{}", uuid::Uuid::new_v4()))
        .collect();
    for model in &models {
        create_model(&server, model).await;
    }

    let changes: Vec<serde_json::Value> = models
        .iter()
        .map(|m| change_item(m, "2030-01-01T00:00:00Z", 1_500))
        .collect();
    let resp = post_pricing_changes(
        &server,
        "confirm",
        serde_json::json!({ "changes": changes }),
    )
    .await;
    assert_eq!(resp.status_code(), 200, "{}", resp.text());
    let body: serde_json::Value = serde_json::from_str(&resp.text()).unwrap();
    let batch_id = body["batchId"].as_str().unwrap().to_string();
    backdate_batch(&database, &batch_id).await;

    // Two scheduler instances racing over the same due set (multi-instance
    // deployment): SKIP LOCKED claims must partition it.
    let scheduler_a = make_scheduler(&database);
    let scheduler_b = make_scheduler(&database);
    let (a, b) = tokio::join!(scheduler_a.run_once(), scheduler_b.run_once());
    a.unwrap();
    b.unwrap();

    let listed = list_pricing_changes(&server, "?status=applied&limit=500").await;
    for model in &models {
        assert!(
            listed["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["modelId"] == model.as_str()),
            "{model} should be applied"
        );

        // Exactly 2 history entries: creation + the single scheduled apply.
        // A double apply would add a third.
        let history_resp = server
            .get(&format!("/v1/admin/models/{model}/history"))
            .add_header("Authorization", format!("Bearer {}", get_session_id()))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await;
        let history: serde_json::Value = serde_json::from_str(&history_resp.text()).unwrap();
        assert_eq!(
            history["history"].as_array().unwrap().len(),
            2,
            "{model} must be applied exactly once: {history}"
        );
    }
}

// ---------------------------------------------------------------------------
// Recipients & consolidated notification
// ---------------------------------------------------------------------------

/// Seed a usage-log row so the org's owner becomes a notification recipient.
async fn seed_usage(
    database: &Arc<database::Database>,
    org_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
    api_key_id: uuid::Uuid,
    model_name: &str,
) {
    let client = database.pool().get().await.unwrap();
    let model_id: uuid::Uuid = client
        .query_one(
            "SELECT id FROM models WHERE model_name = $1",
            &[&model_name],
        )
        .await
        .unwrap()
        .get(0);
    client
        .execute(
            r#"
            INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id,
                model_id, model_name, input_tokens, output_tokens,
                total_tokens, input_cost, output_cost, total_cost,
                inference_type, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, 10, 10, 20, 1, 1, 2,
                      'chat_completion', NOW())
            "#,
            &[
                &uuid::Uuid::new_v4(),
                &org_id,
                &workspace_id,
                &api_key_id,
                &model_id,
                &model_name,
            ],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_preview_counts_and_consolidated_delivery_rows() {
    let (server, database) = setup_test_server_with_database().await;
    let model_a = format!("pricing-change-recip-a-{}", uuid::Uuid::new_v4());
    let model_b = format!("pricing-change-recip-b-{}", uuid::Uuid::new_v4());
    create_model(&server, &model_a).await;
    create_model(&server, &model_b).await;

    // Org owned by the mock admin user, with usage of BOTH models.
    let org = create_org(&server).await;
    let org_id = uuid::Uuid::parse_str(&org.id).unwrap();
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces.first().unwrap();
    let api_key =
        create_api_key_in_workspace(&server, workspace.id.clone(), "pricing-e2e".to_string()).await;
    let workspace_id = uuid::Uuid::parse_str(&workspace.id).unwrap();
    let api_key_id = uuid::Uuid::parse_str(&api_key.id).unwrap();
    seed_usage(&database, org_id, workspace_id, api_key_id, &model_a).await;
    seed_usage(&database, org_id, workspace_id, api_key_id, &model_b).await;

    let changes = serde_json::json!({ "changes": [
        change_item(&model_a, "2030-01-01", 1_500),
        change_item(&model_b, "2030-01-01", 1_500),
    ] });

    // Preview: one distinct recipient (the org owner), one org, per-model counts.
    let preview = post_pricing_changes(&server, "preview", changes.clone()).await;
    assert_eq!(preview.status_code(), 200, "{}", preview.text());
    let preview: serde_json::Value = serde_json::from_str(&preview.text()).unwrap();
    assert_eq!(preview["recipientCount"], 1, "{preview}");
    assert_eq!(preview["organizationCount"], 1, "{preview}");
    assert_eq!(preview["usageWindowDays"], 30);
    for model in preview["models"].as_array().unwrap() {
        assert_eq!(model["recipientCount"], 1, "{preview}");
        assert_eq!(model["organizationCount"], 1, "{preview}");
    }

    // Preview must not have persisted anything.
    let listed = list_pricing_changes(&server, "?status=pending&limit=500").await;
    assert!(!listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["modelId"] == model_a.as_str()));

    // Confirm: ONE consolidated delivery row per (user, org) listing BOTH models.
    let confirm = post_pricing_changes(&server, "confirm", changes).await;
    assert_eq!(confirm.status_code(), 200, "{}", confirm.text());
    let confirm: serde_json::Value = serde_json::from_str(&confirm.text()).unwrap();
    assert_eq!(confirm["recipientCount"], 1, "{confirm}");
    // Email sending is disabled in tests (Noop sender) -> counted as skipped.
    assert_eq!(confirm["skippedCount"], 1, "{confirm}");
    assert_eq!(confirm["sentCount"], 0, "{confirm}");
    let batch_id = uuid::Uuid::parse_str(confirm["batchId"].as_str().unwrap()).unwrap();

    let client = database.pool().get().await.unwrap();
    let rows = client
        .query(
            "SELECT recipient_email, organization_id, model_names, status
             FROM model_pricing_change_email_deliveries
             WHERE batch_id = $1",
            &[&batch_id],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one delivery row per (user, org) membership");
    let model_names: Vec<String> = rows[0].get("model_names");
    let mut expected = vec![model_a.clone(), model_b.clone()];
    expected.sort();
    assert_eq!(model_names, expected, "email must cover both used models");
    let status: String = rows[0].get("status");
    assert_eq!(status, "skipped");
    let row_org: uuid::Uuid = rows[0].get("organization_id");
    assert_eq!(row_org, org_id);
}
