use chrono::{DateTime, TimeZone, Utc};
use database::{migrations, DbPool};
use deadpool::Runtime;
use deadpool_postgres::Config;
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use uuid::Uuid;

static MIGRATED: OnceCell<()> = OnceCell::const_new();

pub struct OrgFixture {
    pub org_id: Uuid,
    pub workspace_a_id: Uuid,
    pub workspace_b_id: Uuid,
    pub api_key_a_id: Uuid,
    pub api_key_b_id: Uuid,
}

#[derive(Clone)]
pub struct ModelFixture {
    pub id: Uuid,
    pub name: String,
}

pub struct UsageSeed {
    pub id: Uuid,
    pub org_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub model: ModelFixture,
    pub created_at: DateTime<Utc>,
    pub inference_type: &'static str,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cache_read_tokens: i32,
    pub input_cost: i64,
    pub output_cost: i64,
    pub total_cost: i64,
    pub response_id: Option<Uuid>,
    pub inference_id: Uuid,
    pub provider_request_id: &'static str,
}

pub fn ts(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
}

pub async fn test_pool() -> anyhow::Result<DbPool> {
    let pool = DbPool::new(pool_config().create_pool(Some(Runtime::Tokio1), NoTls)?);
    MIGRATED
        .get_or_try_init(|| async { migrations::run(&pool).await })
        .await?;
    Ok(pool)
}

pub async fn insert_org_fixture(pool: &DbPool) -> anyhow::Result<OrgFixture> {
    let client = pool.get().await?;
    let org_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let workspace_a_id = Uuid::new_v4();
    let workspace_b_id = Uuid::new_v4();
    let api_key_a_id = Uuid::new_v4();
    let api_key_b_id = Uuid::new_v4();
    let suffix = org_id.simple().to_string();
    let now = Utc::now();

    client
        .execute(
            "INSERT INTO users (id, email, username, created_at, updated_at, is_active, auth_provider, provider_user_id)
             VALUES ($1, $2, $3, $4, $4, true, 'test', $5)",
            &[
                &user_id,
                &format!("usage-report-{suffix}@example.test"),
                &format!("usage-report-{suffix}"),
                &now,
                &format!("provider-{suffix}"),
            ],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
             VALUES ($1, $2, NULL, $3, $3, true)",
            &[&org_id, &format!("usage-report-org-{suffix}"), &now],
        )
        .await?;

    for (workspace_id, name) in [
        (workspace_a_id, "usage-report-a"),
        (workspace_b_id, "usage-report-b"),
    ] {
        client
            .execute(
                "INSERT INTO workspaces (id, name, description, organization_id, created_by_user_id, created_at, updated_at, is_active, settings)
                 VALUES ($1, $2, NULL, $3, $4, $5, $5, true, '{}'::jsonb)",
                &[
                    &workspace_id,
                    &format!("{name}-{suffix}"),
                    &org_id,
                    &user_id,
                    &now,
                ],
            )
            .await?;
    }

    for (api_key_id, workspace_id, name) in [
        (api_key_a_id, workspace_a_id, "usage-report-key-a"),
        (api_key_b_id, workspace_b_id, "usage-report-key-b"),
    ] {
        client
            .execute(
                "INSERT INTO api_keys (id, key_hash, name, workspace_id, created_by_user_id, created_at, is_active, key_prefix)
                 VALUES ($1, $2, $3, $4, $5, $6, true, $7)",
                &[
                    &api_key_id,
                    &format!("hash-{api_key_id}"),
                    &format!("{name}-{suffix}"),
                    &workspace_id,
                    &user_id,
                    &now,
                    &format!("sk-{}", &suffix[..8]),
                ],
            )
            .await?;
    }

    Ok(OrgFixture {
        org_id,
        workspace_a_id,
        workspace_b_id,
        api_key_a_id,
        api_key_b_id,
    })
}

pub async fn insert_model(pool: &DbPool, name: &str) -> anyhow::Result<ModelFixture> {
    let client = pool.get().await?;
    let id = Uuid::new_v4();
    let now = Utc::now();
    client
        .execute(
            "INSERT INTO models (id, model_name, model_display_name, model_description, input_cost_per_token, output_cost_per_token, context_length, verifiable, is_active, created_at, updated_at)
             VALUES ($1, $2, $2, 'reporting test model', 10, 20, 4096, true, true, $3, $3)",
            &[&id, &format!("{name}-{id}"), &now],
        )
        .await?;
    let row = client
        .query_one("SELECT model_name FROM models WHERE id = $1", &[&id])
        .await?;
    Ok(ModelFixture {
        id,
        name: row.get("model_name"),
    })
}

pub async fn insert_usage(pool: &DbPool, seed: &UsageSeed) -> anyhow::Result<()> {
    let client = pool.get().await?;
    if let Some(response_id) = seed.response_id {
        client
            .execute(
                "INSERT INTO responses (id, model, status, instructions, conversation_id, previous_response_id, usage, metadata, created_at, updated_at, workspace_id, api_key_id)
                 VALUES ($1, $2, 'completed', NULL, NULL, NULL, '{}'::jsonb, '{}'::jsonb, $3, $3, $4, $5)",
                &[
                    &response_id,
                    &seed.model.name,
                    &seed.created_at,
                    &seed.workspace_id,
                    &seed.api_key_id,
                ],
            )
            .await?;
    }
    client
        .execute(
            "INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id, response_id, model_id, model_name,
                input_tokens, output_tokens, cache_read_tokens, total_tokens,
                input_cost, output_cost, total_cost, request_type, inference_type, created_at,
                inference_id, provider_request_id, stop_reason
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, NULL, $15, $16, $17, $18, 'completed')",
            &[
                &seed.id,
                &seed.org_id,
                &seed.workspace_id,
                &seed.api_key_id,
                &seed.response_id,
                &seed.model.id,
                &seed.model.name,
                &seed.input_tokens,
                &seed.output_tokens,
                &seed.cache_read_tokens,
                &(seed.input_tokens + seed.output_tokens),
                &seed.input_cost,
                &seed.output_cost,
                &seed.total_cost,
                &seed.inference_type,
                &seed.created_at,
                &Some(seed.inference_id),
                &Some(seed.provider_request_id),
            ],
        )
        .await?;
    Ok(())
}

pub async fn cleanup_usage_fixtures(
    pool: &DbPool,
    org_ids: &[Uuid],
    model_ids: &[Uuid],
) -> anyhow::Result<()> {
    let client = pool.get().await?;
    let org_ids = org_ids.to_vec();
    let model_ids = model_ids.to_vec();
    client
        .execute("DELETE FROM organizations WHERE id = ANY($1)", &[&org_ids])
        .await?;
    client
        .execute("DELETE FROM models WHERE id = ANY($1)", &[&model_ids])
        .await?;
    Ok(())
}

fn pool_config() -> Config {
    let mut config = Config::new();
    config.host = Some(std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_string()));
    config.port = Some(
        std::env::var("PGPORT")
            .ok()
            .and_then(|value| value.parse::<u16>().ok())
            .unwrap_or(5432),
    );
    config.dbname =
        Some(std::env::var("PGDATABASE").unwrap_or_else(|_| "platform_api".to_string()));
    config.user = Some(std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_string()));
    config.password = Some(std::env::var("PGPASSWORD").unwrap_or_else(|_| "postgres".to_string()));
    config
}
