use crate::common::*;
use axum::http::StatusCode;
use database::repositories::PostgresAdminSettingsRepository;
use serde_json::{json, Value};
use services::admin_settings::{AdminSettingsRepository, AdminSettingsService};
use services::usage::budget::policy::{
    AdmissionMode, AdmissionPolicy, AdmissionSettings, UsageAdmissionKey,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use uuid::Uuid;

pub(super) async fn server(
    settings: AdmissionSettings,
) -> (
    axum_test::TestServer,
    Arc<inference_providers::mock::MockProvider>,
    Arc<services::inference_provider_pool::InferenceProviderPool>,
    Arc<database::Database>,
) {
    let (server, mock, pool, database, _) = server_with_policy(settings).await;
    (server, mock, pool, database)
}

type PolicyFixture = (
    axum_test::TestServer,
    Arc<inference_providers::mock::MockProvider>,
    Arc<services::inference_provider_pool::InferenceProviderPool>,
    Arc<database::Database>,
    Arc<AdminSettingsService>,
);

async fn server_with_policy(settings: AdmissionSettings) -> PolicyFixture {
    let fixture = server_with_stored_policy(serde_json::to_value(settings).unwrap()).await;
    assert!(fixture.4.reload().await);
    fixture
}

async fn server_with_stored_policy(value: Value) -> PolicyFixture {
    let database = Arc::new(database::Database::new(db_setup::create_test_pool().await));
    let mut config = test_config();
    config.external_providers.enable_anthropic_messages = true;
    config.external_providers.timeout_seconds = 5;
    config.internal_usage_token = Some("test-internal-secret".into());
    assert_mock_user_in_db(&database).await;
    let auth = api::init_auth_services(database.clone(), &config);
    let (pool, mock) = api::init_inference_providers_with_mocks(&config).await;
    // Each test changes global defaults on its own connection. The actual repository still
    // reads the organization rows and the global setting in one PostgreSQL statement.
    let policy_pool = db_setup::create_test_pool().await;
    policy_pool.current().unwrap().resize(1);
    policy_pool
        .get()
        .await
        .unwrap()
        .batch_execute(
            "CREATE TEMP TABLE admin_settings (LIKE public.admin_settings INCLUDING ALL)",
        )
        .await
        .unwrap();
    let repository = Arc::new(PostgresAdminSettingsRepository::new(policy_pool));
    repository
        .upsert("usage_admission", value, Uuid::new_v4())
        .await
        .unwrap();
    let settings = Arc::new(
        AdminSettingsService::builder(repository)
            .register::<UsageAdmissionKey>(Arc::new(arc_swap::ArcSwap::from_pointee(
                AdmissionSettings::default(),
            )))
            .build(),
    );
    let services = api::init_domain_services_with_pool(
        database.clone(),
        &config,
        auth.organization_service.clone(),
        pool.clone(),
        Arc::new(services::metrics::MockMetricsService),
        Some(settings.clone()),
    )
    .await;
    let app = build_app_with_config_and_options(
        database.clone(),
        auth,
        services,
        Arc::new(config),
        E2E_APP_BUILD_OPTIONS,
    );
    (
        axum_test::TestServer::new(app),
        mock,
        pool,
        database,
        settings,
    )
}

pub(super) async fn assert_clean_settlements(db: &database::Database, org: Uuid, count: i64) {
    let row = db.pool().get().await.unwrap().query_one(
        "SELECT count(*), count(DISTINCT u.id), bool_and(r.state='settled'
         AND r.outcome_unknown_at IS NULL AND r.actual_cost=u.total_cost),
         reserved_credit($1), (SELECT count(*) FROM organization_usage_log WHERE organization_id=$1)
         FROM usage_budget_reservations r LEFT JOIN organization_usage_log u ON u.id=r.inference_usage_id
         WHERE r.organization_id=$1", &[&org]).await.unwrap();
    assert_eq!(row.get::<_, i64>(0), count);
    assert_eq!(row.get::<_, i64>(1), count);
    assert!(row.get::<_, bool>(2));
    assert_eq!(row.get::<_, i64>(3), 0);
    assert_eq!(row.get::<_, i64>(4), count);
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

async fn enforce_organization(db: &database::Database, settings: &AdminSettingsService, id: Uuid) {
    db.pool()
        .get()
        .await
        .unwrap()
        .execute(
            "INSERT INTO organization_usage_admission (organization_id,mode) VALUES ($1,'enforce')",
            &[&id],
        )
        .await
        .unwrap();
    assert!(settings.reload().await);
}

#[tokio::test]
async fn gateway_and_direct_admission_return_503_until_policy_loads() {
    let (server, mock, _, _, settings) =
        server_with_stored_policy(json!({"defaults":{"mode":"invalid"}})).await;
    assert!(!settings.reload().await);
    setup_qwen_model(&server).await;
    let org = setup_org_with_credits(&server, 1_000_000_000).await;
    let ws = list_workspaces(&server, org.id.clone()).await.remove(0);
    let key = create_api_key_in_workspace(&server, ws.id.clone(), "policy-load".into()).await;
    let auth = format!("Bearer {}", key.key.as_ref().unwrap());
    let chat = |stream: bool| {
        server
            .post("/v1/chat/completions")
            .add_header("Authorization", auth.clone())
            .json(&json!({"model": E2E_QWEN_MODEL_NAME, "stream": stream,
            "messages": [{"role":"user","content":"hi"}], "max_tokens": 1}))
    };
    let features = services::usage::budget::UsageFeatures::from_json(
        services::usage::InferenceType::ChatCompletion,
        &json!({"max_tokens":1}),
    )
    .unwrap();
    let direct = || {
        server
            .post("/v1/internal/budget")
            .add_header("Authorization", "Bearer test-internal-secret")
            .json(
                &json!({"id": Uuid::new_v4(), "organization_id": org.id, "workspace_id": ws.id,
            "api_key_id": key.id, "model": E2E_QWEN_MODEL_NAME, "features": features}),
            )
    };

    let refusal = direct().await;
    refusal.assert_status(StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refusal.json::<Value>()["error"]["type"],
        "admission_policy_unavailable"
    );
    for stream in [false, true] {
        chat(stream)
            .await
            .assert_status(StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(mock.chat_completion_call_count(), 0);
    use axum_test::multipart::{MultipartForm, Part};
    server
        .post("/v1/audio/transcriptions")
        .add_header("Authorization", auth.clone())
        .multipart(
            MultipartForm::new()
                .add_part(
                    "file",
                    Part::bytes(include_bytes!("../fixtures/audio/silence.wav").to_vec())
                        .file_name("test.wav")
                        .mime_type("audio/wav"),
                )
                .add_text("model", E2E_QWEN_MODEL_NAME),
        )
        .await
        .assert_status(StatusCode::SERVICE_UNAVAILABLE);

    settings
        .update(
            "usage_admission",
            json!({"defaults":{"mode":"audit"}}),
            Uuid::new_v4(),
        )
        .await
        .unwrap();
    let admitted = direct().await;
    admitted.assert_status_ok();
    assert_eq!(admitted.json::<Value>()["mode"], "audit");
    chat(false).await.assert_status_ok();
    assert_eq!(mock.chat_completion_call_count(), 1);
}

#[tokio::test]
async fn chat_stream_settles_once_without_marking_its_hold_unknown() {
    let (server, _, _, db) = server(enforcing_settings()).await;
    let org = setup_org_with_credits(&server, 10_000_000_000).await;
    let key = get_api_key_for_org(&server, org.id.clone()).await;
    let response = server
        .post("/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {key}"))
        .json(
            &json!({"model":E2E_QWEN_MODEL_NAME,"messages":[{"role":"user","content":"Hi"}],
                       "stream":true,"max_tokens":8}),
        )
        .await;
    response.assert_status_ok();
    assert!(response.text().contains("[DONE]"));
    let org = Uuid::parse_str(&org.id).unwrap();
    let client = db.pool().get().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let posted: bool = client
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM organization_usage_log WHERE organization_id=$1)",
                    &[&org],
                )
                .await
                .unwrap()
                .get(0);
            if posted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stream's background settlement should commit");
    assert_clean_settlements(&db, org, 1).await;
}

#[tokio::test]
async fn native_responses_preserve_capacity_and_unavailable_admission_statuses() {
    let mut settings = enforcing_settings();
    settings.defaults.key_max_unsettled = 1;
    let (server, _, pool, db) = server(settings).await;
    let model = format!("openai/admission-status-{}", Uuid::new_v4());
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let provider = inference_providers::mock::MockProvider::new_accept_all()
        .with_responses_handler(move |_| {
            called.fetch_add(1, Ordering::SeqCst);
            panic!("rejected admission must not dispatch native Responses")
        });
    pool.register_provider(model.clone(), Arc::new(provider))
        .await;
    let mut models = api::models::BatchUpdateModelApiRequest::new();
    models.insert(model.clone(), serde_json::from_value(json!({
        "inputCostPerToken":{"amount":1000,"currency":"USD"},
        "outputCostPerToken":{"amount":2000,"currency":"USD"},
        "modelDisplayName":"Admission status fixture", "modelDescription":"Native admission statuses",
        "contextLength":4096, "maxOutputLength":128, "verifiable":false, "isActive":true,
        "attestationSupported":false
    })).unwrap());
    seed_models(&server, models, get_session_id()).await;
    let org = setup_org_with_credits(&server, 1_000_000_000).await;
    let ws = list_workspaces(&server, org.id.clone()).await.remove(0);
    let key = create_api_key_in_workspace(&server, ws.id.clone(), "admission-status".into()).await;
    let id = Uuid::new_v4();
    let features = services::usage::budget::UsageFeatures::from_json(
        services::usage::InferenceType::ChatCompletion,
        &json!({"max_output_tokens":8}),
    )
    .unwrap();
    server
        .post("/v1/internal/budget")
        .add_header("Authorization", "Bearer test-internal-secret")
        .json(
            &json!({"id":id,"organization_id":org.id,"workspace_id":ws.id,"api_key_id":key.id,
            "model":model,"features":features}),
        )
        .await
        .assert_status_ok();

    for expected in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::SERVICE_UNAVAILABLE,
    ] {
        let mut client = db.pool().get().await.unwrap();
        let lock = client.transaction().await.unwrap();
        if expected == StatusCode::SERVICE_UNAVAILABLE {
            // Hold only this fixture's organization until admission's lock deadline expires.
            lock.query_one(
                "SELECT id FROM organizations WHERE id=$1 FOR UPDATE",
                &[&Uuid::parse_str(&org.id).unwrap()],
            )
            .await
            .unwrap();
        }
        for stream in [false, true] {
            let response = server
                .post("/v1/responses")
                .add_header(
                    "Authorization",
                    format!("Bearer {}", key.key.as_ref().unwrap()),
                )
                .json(&json!({"model":model,"input":"test","max_output_tokens":8,"stream":stream}))
                .await;
            assert_eq!(response.status_code(), expected, "{}", response.text());
            assert!(!response.text().contains("lock timeout"));
            assert!(!response.text().contains("SQLSTATE"));
        }
        lock.rollback().await.unwrap();
        if expected == StatusCode::TOO_MANY_REQUESTS {
            server
                .post("/v1/internal/budget/release")
                .add_header("Authorization", "Bearer test-internal-secret")
                .json(&json!({"organization_id":org.id,"id":id}))
                .await
                .assert_status(StatusCode::NO_CONTENT);
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn global_disable_and_reenable_preserve_direct_admission_policy_and_existing_holds() {
    let (server, _, _, db, settings) = server_with_policy(AdmissionSettings::default()).await;
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
    for (step, expected, expected_holds) in [
        (0, "audit", 0),
        (1, "enforce", 1),
        (2, "off", 1),
        (3, "enforce", 2),
    ] {
        match step {
            1 => enforce_organization(&db, &settings, Uuid::parse_str(&org.id).unwrap()).await,
            2 | 3 => {
                settings
                    .update(
                        "usage_admission",
                        json!({"enabled":step == 3}),
                        Uuid::new_v4(),
                    )
                    .await
                    .unwrap();
            }
            _ => {}
        }
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
    let (server, mock, pool, db, settings) = server_with_policy(AdmissionSettings::default()).await;
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
    enforce_organization(&db, &settings, org_id).await;

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
        settings
            .update("usage_admission", json!({"enabled":false}), Uuid::new_v4())
            .await
            .unwrap();
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
    assert_clean_settlements(&db, org_id, admitted as i64).await;
}

#[tokio::test]
async fn audit_and_off_do_not_reject_chat_on_estimation_metadata() {
    for mode in [AdmissionMode::Audit, AdmissionMode::Off] {
        let settings = AdmissionSettings {
            defaults: AdmissionPolicy {
                mode,
                ..Default::default()
            },
            ..Default::default()
        };
        let (server, _, _, db) = server(settings).await;
        setup_qwen_model(&server).await;
        let org = setup_org_with_credits(&server, 1_000_000_000).await;
        let key = get_api_key_for_org(&server, org.id.clone()).await;
        for stream in [false, true] {
            server.post("/v1/chat/completions")
                .add_header("Authorization", format!("Bearer {key}"))
                .json(&json!({"model":E2E_QWEN_MODEL_NAME, "messages":[{"role":"user","content":"hello"}],
                    "max_tokens":8, "max_output_tokens":-1, "stream":stream}))
                .await.assert_status_ok();
        }
        let count: i64 = db
            .pool()
            .get()
            .await
            .unwrap()
            .query_one(
                "SELECT COUNT(*) FROM usage_budget_reservations WHERE organization_id=$1",
                &[&Uuid::parse_str(&org.id).unwrap()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 0);
    }
}
