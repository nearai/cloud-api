use crate::common::*;
use axum::http::StatusCode;
use serde_json::{json, Value};
use services::admin_settings::{AdminSettingsRepository, AdminSettingsService, StoredSetting};
use services::usage::budget::policy::{
    AdmissionMode, AdmissionPolicy, AdmissionSettings, UsageAdmissionKey,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use uuid::Uuid;

// Keep policy updates local to this server, while exercising the actual registry and routes.
struct SettingsRepository(Mutex<StoredSetting>);

#[async_trait::async_trait]
impl AdminSettingsRepository for SettingsRepository {
    async fn get_all(&self) -> anyhow::Result<Vec<StoredSetting>> {
        Ok(vec![self.0.lock().unwrap().clone()])
    }
    async fn get(&self, key: &str) -> anyhow::Result<Option<StoredSetting>> {
        Ok((key == "usage_admission").then(|| self.0.lock().unwrap().clone()))
    }
    async fn upsert(
        &self,
        key: &str,
        value: Value,
        by_user: Uuid,
    ) -> anyhow::Result<StoredSetting> {
        assert_eq!(key, "usage_admission");
        let row = StoredSetting {
            key: key.into(),
            value,
            updated_by_user_id: Some(by_user),
            updated_at: chrono::Utc::now(),
        };
        *self.0.lock().unwrap() = row.clone();
        Ok(row)
    }
}

pub(super) async fn server(
    settings: AdmissionSettings,
) -> (
    axum_test::TestServer,
    Arc<inference_providers::mock::MockProvider>,
    Arc<services::inference_provider_pool::InferenceProviderPool>,
    Arc<database::Database>,
) {
    let database = Arc::new(database::Database::new(db_setup::create_test_pool().await));
    let mut config = test_config();
    config.internal_usage_token = Some("test-internal-secret".into());
    assert_mock_user_in_db(&database).await;
    let auth = api::init_auth_services(database.clone(), &config);
    let (pool, mock) = api::init_inference_providers_with_mocks(&config).await;
    let settings = Arc::new(
        AdminSettingsService::builder(Arc::new(SettingsRepository(Mutex::new(StoredSetting {
            key: "usage_admission".into(),
            value: serde_json::to_value(settings).unwrap(),
            updated_by_user_id: None,
            updated_at: chrono::Utc::now(),
        }))))
        .register::<UsageAdmissionKey>(Arc::new(arc_swap::ArcSwap::from_pointee(
            AdmissionSettings::default(),
        )))
        .build(),
    );
    assert!(settings.reload().await);
    let services = api::init_domain_services_with_pool(
        database.clone(),
        &config,
        auth.organization_service.clone(),
        pool.clone(),
        Arc::new(services::metrics::MockMetricsService),
        Some(settings),
    )
    .await;
    let app = api::build_app_with_config_and_options(
        database.clone(),
        auth,
        services,
        Arc::new(config),
        E2E_APP_BUILD_OPTIONS,
    );
    (axum_test::TestServer::new(app), mock, pool, database)
}

pub(super) fn enforcing_settings() -> AdmissionSettings {
    AdmissionSettings {
        defaults: AdmissionPolicy {
            mode: AdmissionMode::Enforce,
            ..Default::default()
        },
        ..Default::default()
    }
}

pub(super) async fn enforcing_server() -> axum_test::TestServer {
    server(enforcing_settings()).await.0
}

async fn policy(server: &axum_test::TestServer, value: Value) {
    server
        .patch("/v1/admin/settings/usage_admission")
        .add_header("Authorization", format!("Bearer {}", get_session_id()))
        .add_header("User-Agent", MOCK_USER_AGENT)
        .json(&value)
        .await
        .assert_status_ok();
}

#[tokio::test]
async fn global_disable_and_reenable_preserve_direct_admission_policy_and_existing_holds() {
    let (server, _, _, db) = server(AdmissionSettings::default()).await;
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 1_000_000_000).await;
    let ws = list_workspaces(&server, org.id.clone()).await.remove(0);
    let key = create_api_key_in_workspace(&server, ws.id.clone(), "mode-test".into()).await;
    let id = Uuid::new_v4();
    let features = services::usage::budget::UsageFeatures::from_json(
        services::usage::InferenceType::ChatCompletion,
        &json!({"max_tokens":1}),
    )
    .unwrap();
    let body = json!({"id":id,"organization_id":org.id,"workspace_id":ws.id,"api_key_id":key.id,
        "model":E2E_QWEN_MODEL_NAME,"features":features});
    for (change, expected, expected_holds) in [
        (json!({}), "audit", 0),
        (
            json!({"organizations":{org.id.clone():{"mode":"enforce"}}}),
            "enforce",
            1,
        ),
        (json!({"enabled":false}), "off", 1),
        (json!({"enabled":true}), "enforce", 2),
    ] {
        policy(&server, change).await;
        let mut body = body.clone();
        body["id"] = json!(Uuid::new_v4());
        let response = server
            .post("/v1/internal/budget")
            .add_header("Authorization", "Bearer test-internal-secret")
            .json(&body)
            .await;
        response.assert_status_ok();
        let response: Value = response.json();
        assert_eq!(response["mode"], expected);
        assert_eq!(response["id"].is_string(), expected == "enforce");
        assert_eq!(response["estimated_amount"].is_null(), expected == "off");
        let count: i64 = db
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM usage_budget_reservations WHERE organization_id=$1 AND state='active'",
                &[&Uuid::parse_str(&org.id).unwrap()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, expected_holds, "disabling preserves existing holds");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_concurrent_requests_share_a_small_key_budget_and_settle_actual_spend() {
    let (server, mock, pool, db) = server(AdmissionSettings::default()).await;
    let model = format!("admission-test-{}", Uuid::new_v4());
    pool.register_provider(model.clone(), mock.clone()).await;
    let mut models = api::models::BatchUpdateModelApiRequest::new();
    models.insert(model.clone(), serde_json::from_value(json!({
        "inputCostPerToken":{"amount":10_000,"currency":"USD"},
        "outputCostPerToken":{"amount":20_000,"currency":"USD"},
        "modelDisplayName":"Admission test", "modelDescription":"Deterministic accounting fixture",
        "contextLength":4096, "maxOutputLength":128, "verifiable":false, "isActive":true,
        "attestationSupported":false
    })).unwrap());
    assert_eq!(
        seed_models(&server, models, get_session_id()).await.len(),
        1
    );
    let org = setup_org_with_credits(&server, 1_000_000_000).await;
    let ws = list_workspaces(&server, org.id.clone()).await.remove(0);
    let key = create_api_key_in_workspace(&server, ws.id.clone(), "concurrent-budget".into()).await;
    let key_id = Uuid::parse_str(&key.id).unwrap();
    let org_id = Uuid::parse_str(&org.id).unwrap();
    const KEY_LIMIT: i64 = 10_000_000;
    db.pool()
        .get()
        .await
        .unwrap()
        .execute(
            "UPDATE api_keys SET spend_limit=$2 WHERE id=$1",
            &[&key_id, &KEY_LIMIT],
        )
        .await
        .unwrap();
    policy(
        &server,
        json!({"organizations":{org.id.clone():{"mode":"enforce"}}}),
    )
    .await;

    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    mock.set_default_response(
        inference_providers::mock::ResponseTemplate::new("word ".repeat(64))
            .with_completion_gate(gate.clone()),
    )
    .await;
    let start = tokio::sync::Barrier::new(10);
    let completed = AtomicUsize::new(0);
    let requests = (0..10).map(|_| {
        let server = &server; let start = &start; let completed = &completed;
        let model = &model; let secret = key.key.as_ref().unwrap();
        async move {
            start.wait().await;
            let response = server.post("/v1/chat/completions")
                .add_header("Authorization", format!("Bearer {secret}"))
                .json(&json!({"model":model,"messages":[{"role":"user","content":"one two three four five six"}],"max_tokens":64,"stream":false})).await;
            completed.fetch_add(1, Ordering::SeqCst);
            (response.status_code(), response.text())
        }
    });
    let observer = async {
        tokio::time::timeout(Duration::from_secs(15), async {
            while mock.chat_completion_call_count() + completed.load(Ordering::SeqCst) < 10 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("all admissions finish while admitted responses are held");
        let admitted = mock.chat_completion_call_count();
        assert!(
            admitted > 0 && admitted < 10,
            "the budget admits some, but not all requests"
        );
        let row = db.pool().get().await.unwrap().query_one(
            "SELECT COUNT(*), COALESCE(SUM(amount),0)::BIGINT, MIN(amount), MAX(amount) FROM usage_budget_reservations WHERE api_key_id=$1 AND state='active'", &[&key_id],
        ).await.unwrap();
        let (count, held, amount, max): (i64, i64, i64, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3));
        assert_eq!(count as usize, admitted);
        assert_eq!(amount, max);
        assert_eq!(
            count,
            KEY_LIMIT / amount,
            "all available budget is allocated atomically"
        );
        assert!(held <= KEY_LIMIT && held + amount > KEY_LIMIT);
        let spent: i64 = db
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT total_spent FROM organization_balance WHERE organization_id=$1",
                &[&org_id],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(spent, 0, "no actual bill is posted before responses finish");
        // Stopping new admission must not prevent already admitted usage from settling.
        policy(&server, json!({"enabled":false})).await;
        gate.add_permits(10);
        admitted
    };
    let (responses, admitted) = tokio::join!(futures::future::join_all(requests), observer);
    let mut actual = 0_i64;
    for (status, body) in &responses {
        if *status == StatusCode::OK {
            let body: Value = serde_json::from_str(body).unwrap();
            let cost = body["usage"]["prompt_tokens"].as_i64().unwrap() * 10_000
                + body["usage"]["completion_tokens"].as_i64().unwrap() * 20_000;
            assert!(
                cost * 10 > KEY_LIMIT,
                "admitting all ten requests would exceed the key limit"
            );
            actual += cost;
        } else {
            assert_eq!(*status, StatusCode::PAYMENT_REQUIRED, "{body}");
        }
    }
    assert_eq!(
        responses
            .iter()
            .filter(|(s, _)| *s == StatusCode::OK)
            .count(),
        admitted
    );
    assert_eq!(
        mock.chat_completion_call_count(),
        admitted,
        "rejected requests never reach inference"
    );
    assert!(actual > 0 && actual <= KEY_LIMIT);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let client = db.pool().get().await.unwrap();
            let row = client.query_one("SELECT COUNT(*), COALESCE(SUM(total_cost),0)::BIGINT FROM organization_usage_log WHERE api_key_id=$1", &[&key_id]).await.unwrap();
            if row.get::<_,i64>(0) == admitted as i64 {
                assert_eq!(row.get::<_,i64>(1), actual);
                let row = client.query_one("SELECT total_spent,reserved_credit(organization_id) FROM organization_balance WHERE organization_id=$1", &[&org_id]).await.unwrap();
                assert_eq!((row.get::<_,i64>(0),row.get::<_,i64>(1)),(actual,0));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("actual usage settles and releases all holds");
}
