use crate::common::*;
use std::sync::OnceLock;

static HARNESS_ENV: OnceLock<()> = OnceLock::new();

pub(super) struct PlatformProviderUsageFixture {
    pub(super) _server: axum_test::TestServer,
    pub(super) database: std::sync::Arc<database::Database>,
    pub(super) organization_id: uuid::Uuid,
    pub(super) workspace_id: uuid::Uuid,
    pub(super) api_key_id: uuid::Uuid,
    pub(super) model_id: uuid::Uuid,
    pub(super) model_name: String,
}

pub(super) async fn setup_platform_provider_usage_fixture() -> PlatformProviderUsageFixture {
    ensure_platform_provider_usage_harness_env();
    let (server, database) = setup_test_server_with_mock_web_search().await;
    let org = create_org(&server).await;
    let workspaces = list_workspaces(&server, org.id.clone()).await;
    let workspace = workspaces
        .first()
        .expect("organization has default workspace");
    let api_key = create_api_key_in_workspace(
        &server,
        workspace.id.clone(),
        "Provider Usage Metrics Key".to_string(),
    )
    .await;

    let organization_id = uuid::Uuid::parse_str(&org.id).expect("organization id is uuid");
    let workspace_id = uuid::Uuid::parse_str(&workspace.id).expect("workspace id is uuid");
    let api_key_id = uuid::Uuid::parse_str(&api_key.id).expect("api key id is uuid");
    let model_name = format!("test/provider-usage-{}", uuid::Uuid::new_v4());

    let client = database.pool().get().await.expect("db connection");
    let model_id: uuid::Uuid = client
        .query_one(
            r#"
            INSERT INTO models (
                model_name, model_display_name, model_description,
                input_cost_per_token, output_cost_per_token, context_length, max_output_length,
                verifiable, is_active, provider_type, attestation_supported,
                created_at, updated_at
            )
            VALUES ($1, $2, $3, 1, 1, 4096, 1024, true, true, 'external', false, NOW(), NOW())
            RETURNING id
            "#,
            &[&model_name, &model_name, &"Provider usage fixture model"],
        )
        .await
        .expect("insert model")
        .get(0);
    drop(client);

    PlatformProviderUsageFixture {
        _server: server,
        database,
        organization_id,
        workspace_id,
        api_key_id,
        model_id,
        model_name,
    }
}

pub(super) fn ensure_platform_provider_usage_harness_env() {
    HARNESS_ENV.get_or_init(|| {
        std::env::set_var("DEV", "1");
        std::env::set_var("BRAVE_SEARCH_PRO_API_KEY", "test");
    });
}
