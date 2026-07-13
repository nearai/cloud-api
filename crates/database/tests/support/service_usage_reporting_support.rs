use chrono::Utc;
use database::DbPool;
use uuid::Uuid;

pub use crate::service_usage_reporting_pool::test_pool;
pub use crate::service_usage_reporting_rows::ts;
use crate::service_usage_reporting_rows::{insert_usage, service_usage_rows, ServiceUsageRowIds};

pub struct UsageFixture {
    pub org_id: Uuid,
    pub other_org_id: Uuid,
    pub workspace_id: Uuid,
    pub api_key_id: Uuid,
    pub web_search_id: Uuid,
    pub web_search_name: String,
    pub same_time_high_id: Uuid,
    pub same_time_low_id: Uuid,
    pub older_id: Uuid,
}

struct WorkspaceSeed<'a> {
    org_id: Uuid,
    user_id: Uuid,
    suffix: &'a str,
}

struct ApiKeySeed<'a> {
    workspace_id: Uuid,
    user_id: Uuid,
    suffix: &'a str,
}

pub async fn seed_usage_fixture(pool: &DbPool) -> anyhow::Result<UsageFixture> {
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = insert_user(pool, &suffix).await?;
    let org_id = insert_org(pool, &suffix).await?;
    let other_org_id = insert_org(pool, &format!("{suffix}-other")).await?;
    let workspace_id = insert_workspace(
        pool,
        WorkspaceSeed {
            org_id,
            user_id,
            suffix: &suffix,
        },
    )
    .await?;
    let other_workspace_id = insert_workspace(
        pool,
        WorkspaceSeed {
            org_id,
            user_id,
            suffix: &format!("{suffix}-ws"),
        },
    )
    .await?;
    let other_org_workspace_id = insert_workspace(
        pool,
        WorkspaceSeed {
            org_id: other_org_id,
            user_id,
            suffix: &format!("{suffix}-other-ws"),
        },
    )
    .await?;
    let api_key_id = insert_api_key(
        pool,
        ApiKeySeed {
            workspace_id,
            user_id,
            suffix: &suffix,
        },
    )
    .await?;
    let other_api_key_id = insert_api_key(
        pool,
        ApiKeySeed {
            workspace_id: other_workspace_id,
            user_id,
            suffix: &format!("{suffix}-key"),
        },
    )
    .await?;
    let other_org_api_key_id = insert_api_key(
        pool,
        ApiKeySeed {
            workspace_id: other_org_workspace_id,
            user_id,
            suffix: &format!("{suffix}-other-key"),
        },
    )
    .await?;
    let web_search_name = format!("web_search_{suffix}");
    let web_search_id = insert_service(pool, &web_search_name).await?;
    let file_search_id = insert_service(pool, &format!("file_search_{suffix}")).await?;
    let latest_id = Uuid::new_v4();
    let first_same_time_id = Uuid::new_v4();
    let second_same_time_id = Uuid::new_v4();
    let same_time_high_id = first_same_time_id.max(second_same_time_id);
    let same_time_low_id = first_same_time_id.min(second_same_time_id);
    let older_id = Uuid::new_v4();

    for row in service_usage_rows(ServiceUsageRowIds {
        latest_id,
        same_time_high_id,
        same_time_low_id,
        older_id,
        org_id,
        workspace_id,
        api_key_id,
        web_search_id,
        other_workspace_id,
        other_api_key_id,
        file_search_id,
        other_org_id,
        other_org_workspace_id,
        other_org_api_key_id,
    }) {
        insert_usage(pool, &row).await?;
    }

    Ok(UsageFixture {
        org_id,
        other_org_id,
        workspace_id,
        api_key_id,
        web_search_id,
        web_search_name,
        same_time_high_id,
        same_time_low_id,
        older_id,
    })
}

async fn insert_user(pool: &DbPool, suffix: &str) -> anyhow::Result<Uuid> {
    let user_id = Uuid::new_v4();
    let now = Utc::now();
    pool.get()
        .await?
        .execute(
            r#"
            INSERT INTO users (
                id, email, username, display_name, avatar_url, created_at, updated_at,
                last_login_at, is_active, auth_provider, provider_user_id
            )
            VALUES ($1, $2, $3, NULL, NULL, $4, $4, NULL, true, 'test', $5)
            "#,
            &[
                &user_id,
                &format!("service-report-{suffix}@example.test"),
                &format!("service-report-{suffix}"),
                &now,
                &format!("provider-{suffix}"),
            ],
        )
        .await?;
    Ok(user_id)
}

async fn insert_org(pool: &DbPool, suffix: &str) -> anyhow::Result<Uuid> {
    let org_id = Uuid::new_v4();
    let now = Utc::now();
    pool.get()
        .await?
        .execute(
            r#"
            INSERT INTO organizations (id, name, description, created_at, updated_at, is_active)
            VALUES ($1, $2, NULL, $3, $3, true)
            "#,
            &[&org_id, &format!("service-report-org-{suffix}"), &now],
        )
        .await?;
    Ok(org_id)
}

async fn insert_workspace(pool: &DbPool, seed: WorkspaceSeed<'_>) -> anyhow::Result<Uuid> {
    let workspace_id = Uuid::new_v4();
    let now = Utc::now();
    pool.get()
        .await?
        .execute(
            r#"
            INSERT INTO workspaces (
                id, name, description, organization_id, created_by_user_id,
                created_at, updated_at, is_active, settings
            )
            VALUES ($1, $2, NULL, $3, $4, $5, $5, true, '{}'::jsonb)
            "#,
            &[
                &workspace_id,
                &format!("service-report-workspace-{}", seed.suffix),
                &seed.org_id,
                &seed.user_id,
                &now,
            ],
        )
        .await?;
    Ok(workspace_id)
}

async fn insert_api_key(pool: &DbPool, seed: ApiKeySeed<'_>) -> anyhow::Result<Uuid> {
    let api_key_id = Uuid::new_v4();
    let now = Utc::now();
    let key_prefix = format!("sk-{}", &seed.suffix[..8]);
    pool.get()
        .await?
        .execute(
            r#"
            INSERT INTO api_keys (
                id, key_hash, key_prefix, name, workspace_id, created_by_user_id,
                created_at, expires_at, last_used_at, is_active, deleted_at, spend_limit
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, NULL, true, NULL, NULL)
            "#,
            &[
                &api_key_id,
                &format!("service-report-hash-{}", seed.suffix),
                &key_prefix,
                &format!("service-report-key-{}", seed.suffix),
                &seed.workspace_id,
                &seed.user_id,
                &now,
            ],
        )
        .await?;
    Ok(api_key_id)
}

async fn insert_service(pool: &DbPool, service_name: &str) -> anyhow::Result<Uuid> {
    let service_id = Uuid::new_v4();
    let now = Utc::now();
    pool.get()
        .await?
        .execute(
            r#"
            INSERT INTO services (
                id, service_name, display_name, description, unit, cost_per_unit,
                is_active, created_at, updated_at
            )
            VALUES ($1, $2, $3, NULL, 'request', 100, true, $4, $4)
            "#,
            &[&service_id, &service_name, &service_name, &now],
        )
        .await?;
    Ok(service_id)
}
