use crate::common::*;
use database::{models::RecordUsageRequest, repositories::OrganizationUsageRepository};
use services::usage::{InferenceType, ServedProviderTier, ServedProviderType};
use std::sync::Arc;
use uuid::Uuid;

struct ProviderUsageFixture {
    database: Arc<database::Database>,
    organization_id: Uuid,
    workspace_id: Uuid,
    api_key_id: Uuid,
    model_id: Uuid,
    model_name: String,
}

async fn setup_provider_usage_fixture() -> ProviderUsageFixture {
    let pool = db_setup::create_test_pool().await;
    let database = Arc::new(database::Database::new(pool));
    assert_mock_user_in_db(&database).await;
    let client = database
        .pool()
        .get()
        .await
        .expect("database connection should be available");
    let user_id = Uuid::parse_str(MOCK_USER_ID).expect("mock user id should be a UUID");
    let model_name = format!("provider-attribution/{}", Uuid::new_v4());
    let row = client
        .query_one(
            "WITH org AS (
                INSERT INTO organizations (name, description)
                VALUES ($1, 'provider attribution test org')
                RETURNING id
             ), workspace AS (
                INSERT INTO workspaces (name, description, organization_id, created_by_user_id)
                SELECT $2, 'provider attribution test workspace', org.id, $3 FROM org
                RETURNING id
             ), api_key AS (
                INSERT INTO api_keys (key_hash, key_prefix, name, workspace_id, created_by_user_id)
                SELECT lpad(replace(workspace.id::text, '-', ''), 64, '0'), 'sk-test', $4, workspace.id, $3
                FROM workspace
                RETURNING id
             ), model AS (
                INSERT INTO models (
                    model_name, model_display_name, model_description,
                    input_cost_per_token, output_cost_per_token, context_length, max_output_length, verifiable, is_active
                ) VALUES ($5, 'Provider Attribution Model', 'Provider attribution storage test model',
                    1000000, 2000000, 128000, 1024, true, true)
                RETURNING id
             )
             SELECT org.id AS organization_id, workspace.id AS workspace_id,
                    api_key.id AS api_key_id, model.id AS model_id
             FROM org, workspace, api_key, model",
            &[
                &format!("provider-attribution-{}", Uuid::new_v4()),
                &format!("provider-attribution-{}", Uuid::new_v4()),
                &user_id,
                &format!("provider-attribution-{}", Uuid::new_v4()),
                &model_name,
            ],
        )
        .await
        .expect("fixture rows should insert");

    ProviderUsageFixture {
        database,
        organization_id: row.get("organization_id"),
        workspace_id: row.get("workspace_id"),
        api_key_id: row.get("api_key_id"),
        model_id: row.get("model_id"),
        model_name,
    }
}

fn attributed_usage_request(
    fixture: &ProviderUsageFixture,
    inference_id: Uuid,
) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id: fixture.organization_id,
        workspace_id: fixture.workspace_id,
        api_key_id: fixture.api_key_id,
        model_id: fixture.model_id,
        model_name: fixture.model_name.clone(),
        input_tokens: 12,
        output_tokens: 8,
        input_cost: 12_000_000,
        output_cost: 16_000_000,
        total_cost: 28_000_000,
        inference_type: InferenceType::ChatCompletion.as_str().to_string(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(inference_id),
        provider_request_id: Some(format!("provider-attribution-{inference_id}")),
        stop_reason: None,
        response_id: None,
        image_count: None,
        cache_read_tokens: 0,
        served_provider_tier: Some(ServedProviderTier::Attested3p),
        served_provider_type: Some(ServedProviderType::Chutes),
        served_via_fallback: true,
    }
}

async fn insert_raw_attribution_row(
    fixture: &ProviderUsageFixture,
    tier: &str,
    provider_type: &str,
) -> Result<u64, tokio_postgres::Error> {
    let client = fixture
        .database
        .pool()
        .get()
        .await
        .expect("database connection should be available");
    client
        .execute(
            "INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id,
                model_id, model_name, input_tokens, output_tokens, cache_read_tokens, total_tokens,
                input_cost, output_cost, total_cost, inference_type, created_at, inference_id,
                served_provider_tier, served_provider_type, served_via_fallback
             ) VALUES (
                $1, $2, $3, $4, $5, $6, 1, 1, 0, 2,
                1, 1, 2, $7, NOW(), $8, $9, $10, true
             )",
            &[
                &Uuid::new_v4(),
                &fixture.organization_id,
                &fixture.workspace_id,
                &fixture.api_key_id,
                &fixture.model_id,
                &fixture.model_name,
                &InferenceType::ChatCompletion.as_str(),
                &Some(Uuid::new_v4()),
                &tier,
                &provider_type,
            ],
        )
        .await
}

#[tokio::test]
async fn usage_provider_attribution_round_trips() {
    // Given: an e2e database migrated through the current SQL set and a usage repository.
    let fixture = setup_provider_usage_fixture().await;
    let repository = OrganizationUsageRepository::new(fixture.database.pool().clone());
    let attributed_inference_id = Uuid::new_v4();

    // When: one attributed row and one legacy/default row are recorded.
    let attributed = repository
        .record_usage(attributed_usage_request(&fixture, attributed_inference_id))
        .await
        .expect("attributed usage should insert");

    let mut legacy_request = attributed_usage_request(&fixture, Uuid::new_v4());
    legacy_request.served_provider_tier = None;
    legacy_request.served_provider_type = None;
    legacy_request.served_via_fallback = false;
    let legacy = repository
        .record_usage(legacy_request)
        .await
        .expect("legacy/default usage should insert");

    // Then: repository reads and direct SQL rows expose the persisted attribution.
    assert_eq!(
        attributed.served_provider_tier,
        Some(ServedProviderTier::Attested3p)
    );
    assert_eq!(
        attributed.served_provider_type,
        Some(ServedProviderType::Chutes)
    );
    assert!(attributed.served_via_fallback);
    assert_eq!(legacy.served_provider_tier, None);
    assert!(!legacy.served_via_fallback);

    let rows = repository
        .get_usage_history(fixture.organization_id, Some(10), Some(0))
        .await
        .expect("usage history should read");
    assert!(rows.iter().any(|row| {
        row.id == attributed.id
            && row.served_provider_tier == Some(ServedProviderTier::Attested3p)
            && row.served_provider_type == Some(ServedProviderType::Chutes)
            && row.served_via_fallback
    }));
    assert!(rows.iter().any(|row| {
        row.id == legacy.id
            && row.served_provider_tier.is_none()
            && row.served_provider_type.is_none()
            && !row.served_via_fallback
    }));
}

#[tokio::test]
async fn usage_provider_attribution_rejects_invalid_tier_accepts_chutes_type() {
    // Given: a migrated e2e database with valid org/workspace/api-key/model references.
    let fixture = setup_provider_usage_fixture().await;
    // When: a row uses chutes as a provider tier but a valid provider type.
    let invalid_tier = insert_raw_attribution_row(&fixture, "chutes", "chutes").await;

    // Then: the tier check rejects chutes as a tier.
    assert!(
        invalid_tier.is_err(),
        "served_provider_tier='chutes' must be rejected"
    );

    // When: chutes is used as the provider type with a valid tier.
    let valid_type = insert_raw_attribution_row(&fixture, "attested_3p", "chutes").await;

    // Then: the valid provider type is accepted.
    assert_eq!(
        valid_type.expect("served_provider_type='chutes' should be accepted"),
        1
    );
}

#[tokio::test]
async fn duplicate_usage_preserves_original_provider_attribution() {
    // Given: an existing usage row keyed by (organization_id, inference_id).
    let fixture = setup_provider_usage_fixture().await;
    let repository = OrganizationUsageRepository::new(fixture.database.pool().clone());
    let inference_id = Uuid::new_v4();
    let first = repository
        .record_usage(attributed_usage_request(&fixture, inference_id))
        .await
        .expect("first usage should insert");

    // When: a duplicate write carries different costs and attribution.
    let mut duplicate_request = attributed_usage_request(&fixture, inference_id);
    duplicate_request.input_tokens = 99;
    duplicate_request.output_tokens = 99;
    duplicate_request.total_cost = 99;
    duplicate_request.served_provider_tier = Some(ServedProviderTier::Near);
    duplicate_request.served_provider_type = Some(ServedProviderType::Vllm);
    duplicate_request.served_via_fallback = false;
    let duplicate = repository
        .record_usage(duplicate_request)
        .await
        .expect("duplicate usage should return existing row");

    // Then: the first row is returned unchanged and the balance is not charged again.
    assert_eq!(duplicate.id, first.id);
    assert!(!duplicate.was_inserted);
    assert_eq!(duplicate.input_tokens, 12);
    assert_eq!(duplicate.output_tokens, 8);
    assert_eq!(duplicate.total_cost, 28_000_000);
    assert_eq!(
        duplicate.served_provider_tier,
        Some(ServedProviderTier::Attested3p)
    );
    assert_eq!(
        duplicate.served_provider_type,
        Some(ServedProviderType::Chutes)
    );
    assert!(duplicate.served_via_fallback);

    let client = fixture
        .database
        .pool()
        .get()
        .await
        .expect("database connection should be available");
    let row = client
        .query_one(
            "SELECT COUNT(*)::BIGINT AS row_count, SUM(total_cost)::BIGINT AS total_cost
             FROM organization_usage_log
             WHERE organization_id = $1 AND inference_id = $2",
            &[&fixture.organization_id, &Some(inference_id)],
        )
        .await
        .expect("duplicate query should run");
    assert_eq!(row.get::<_, i64>("row_count"), 1);
    assert_eq!(row.get::<_, Option<i64>>("total_cost"), Some(28_000_000));

    let balance = repository
        .get_balance(fixture.organization_id)
        .await
        .expect("balance query should run")
        .expect("balance should exist");
    assert_eq!(balance.total_spent, 28_000_000);
    assert_eq!(balance.total_requests, 1);
}
