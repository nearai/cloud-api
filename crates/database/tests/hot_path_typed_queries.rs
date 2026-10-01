//! Each hot-path repository method that uses `query_typed*` / `execute_typed`
//! is called repeatedly on one pool. A wrong parameter `Type` fails at runtime,
//! so this proves the declared types match the columns.

use chrono::Utc;
use database::repositories::{
    ApiKeyRepository, ModelRepository, OrganizationStakingFarmSourcesRepository,
    OrganizationUsageRepository, WorkspaceRepository,
};
use database::{migrations, DbPool, PgAttestationRepository, PgOrganizationRepository};
use deadpool::Runtime;
use deadpool_postgres::Config;
use services::attestation::{ports::AttestationRepository as _, ChatSignature, SignatureKind};
use services::common::hash_api_key;
use services::completions::ports::OrganizationConcurrentLimitRepository as _;
use services::staking_farm::{StakingFarmRepository as _, UpsertStakingFarmSourceRequest};
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;
use uuid::Uuid;

static MIGRATED: OnceCell<()> = OnceCell::const_new();

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

async fn test_pool() -> anyhow::Result<DbPool> {
    let pool = DbPool::new(pool_config().create_pool(Some(Runtime::Tokio1), NoTls)?);
    MIGRATED
        .get_or_try_init(|| async { migrations::run(&pool).await })
        .await?;
    Ok(pool)
}

fn signature(algo: &str, text: &str, kind: Option<SignatureKind>) -> ChatSignature {
    ChatSignature {
        text: text.to_string(),
        signature: format!("sig-{text}"),
        signing_address: format!("addr-{algo}"),
        signing_algo: algo.to_string(),
        signature_kind: kind,
    }
}

#[tokio::test]
async fn typed_hot_path_queries_repeat_on_one_pool() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let suffix = Uuid::new_v4().simple().to_string();
    let (user_id, org_id, workspace_id, key_id, model_id) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let api_key = format!("sk-live-typed-{suffix}");
    let model_name = format!("typed/model-{suffix}");
    let alias_name = format!("typed-alias-{suffix}");
    let now = Utc::now();

    {
        let client = pool.get().await?;
        client
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
                    &format!("typed-{suffix}@example.test"),
                    &format!("typed-{suffix}"),
                    &now,
                    &format!("provider-{suffix}"),
                ],
            )
            .await?;
        client
            .execute(
                "INSERT INTO organizations (id, name, description, created_at, updated_at, is_active, rate_limit) VALUES ($1, $2, NULL, $3, $3, true, 7)",
                &[&org_id, &format!("typed-org-{suffix}"), &now],
            )
            .await?;
        client
            .execute(
                "INSERT INTO workspaces (id, name, organization_id, created_by_user_id) VALUES ($1, $2, $3, $4)",
                &[&workspace_id, &format!("typed-ws-{suffix}"), &org_id, &user_id],
            )
            .await?;
        client
            .execute(
                "INSERT INTO api_keys (id, key_hash, key_prefix, name, workspace_id, created_by_user_id) VALUES ($1, $2, 'sk-live-typed', 'typed', $3, $4)",
                &[&key_id, &hash_api_key(&api_key), &workspace_id, &user_id],
            )
            .await?;
        client
            .execute(
                "INSERT INTO models (id, model_name, model_display_name, model_description) VALUES ($1, $2, 'Typed', 'typed test model')",
                &[&model_id, &model_name],
            )
            .await?;
        client
            .execute(
                "INSERT INTO model_aliases (alias_name, canonical_model_id) VALUES ($1, $2)",
                &[&alias_name, &model_id],
            )
            .await?;
    }

    // api_key::validate (select) and update_last_used (execute)
    let api_keys = ApiKeyRepository::new(pool.clone());
    for _ in 0..3 {
        let key = api_keys.validate(&api_key).await?.expect("key validates");
        assert_eq!(key.id, key_id);
        assert_eq!(key.workspace_id, workspace_id);
    }
    assert!(api_keys.validate("sk-live-missing").await?.is_none());
    let last_used = pool
        .get()
        .await?
        .query_one(
            "SELECT last_used_at FROM api_keys WHERE id = $1",
            &[&key_id],
        )
        .await?
        .get::<_, Option<chrono::DateTime<Utc>>>(0);
    assert!(last_used.is_some());

    // workspace::get_workspace_with_organization
    let workspaces = WorkspaceRepository::new(pool.clone());
    for _ in 0..3 {
        let (workspace, organization) = workspaces
            .get_workspace_with_organization(workspace_id)
            .await?
            .expect("workspace resolves");
        assert_eq!(workspace.id, workspace_id);
        assert_eq!(organization.id, org_id);
    }
    assert!(workspaces
        .get_workspace_with_organization(Uuid::new_v4())
        .await?
        .is_none());

    // staking source lookup: none, then present
    let staking = OrganizationStakingFarmSourcesRepository::new(pool.clone());
    for _ in 0..2 {
        assert!(staking.get_source_by_organization(org_id).await?.is_none());
    }
    let created = staking
        .upsert_source(UpsertStakingFarmSourceRequest {
            organization_id: org_id,
            near_account_id: format!("typed-{suffix}.near"),
            network_id: "testnet".to_string(),
            contract_id: "farm.test".to_string(),
            farm_product_id: "product".to_string(),
            farm_price_id: None,
            credit_nano_usd_per_reward_unit: 1,
            created_by_user_id: Some(user_id),
        })
        .await?;
    for _ in 0..3 {
        let found = staking
            .get_source_by_organization(org_id)
            .await?
            .expect("source found");
        assert_eq!(found.id, created.id);
    }

    // organization_usage::get_balance and get_api_key_spend
    let usage = OrganizationUsageRepository::new(pool.clone());
    for _ in 0..3 {
        let balance = usage.get_balance(org_id).await?.expect("balance row");
        assert_eq!(balance.organization_id, org_id);
        assert_eq!(balance.total_spent, 0);
        assert_eq!(usage.get_api_key_spend(key_id).await?, 0);
    }
    assert!(usage.get_balance(Uuid::new_v4()).await?.is_none());

    // model::resolve_and_get_model by canonical name and by alias
    let models = ModelRepository::new(pool.clone());
    for _ in 0..3 {
        for identifier in [&model_name, &alias_name] {
            let model = models
                .resolve_and_get_model(identifier)
                .await?
                .expect("model resolves");
            assert_eq!(model.id, model_id);
            assert_eq!(model.aliases, vec![alias_name.clone()]);
        }
    }
    assert!(models
        .resolve_and_get_model(&format!("missing-{suffix}"))
        .await?
        .is_none());

    // organization concurrent limit
    let orgs = PgOrganizationRepository::new(pool.clone());
    for _ in 0..3 {
        assert_eq!(orgs.get_concurrent_limit(org_id).await?, Some(7));
    }
    assert_eq!(orgs.get_concurrent_limit(Uuid::new_v4()).await?, None);

    // attestation: single insert (twice, upsert) and batch insert
    let attestation = PgAttestationRepository::new(pool.clone());
    let chat_id = format!("chatcmpl-typed-{suffix}");
    for text in ["first", "second"] {
        attestation
            .add_chat_signature(
                &chat_id,
                signature("ecdsa", text, Some(SignatureKind::Gateway)),
            )
            .await?;
        let stored = attestation.get_chat_signature(&chat_id, "ecdsa").await?;
        assert_eq!(stored.text, text);
        assert_eq!(stored.signature_kind, Some(SignatureKind::Gateway));
    }
    let batch_chat_id = format!("chatcmpl-typed-batch-{suffix}");
    for text in ["one", "two"] {
        attestation
            .add_chat_signatures(
                &batch_chat_id,
                vec![
                    signature("ecdsa", text, Some(SignatureKind::ProviderTee)),
                    signature("ed25519", text, None),
                ],
            )
            .await?;
        for (algo, kind) in [
            ("ecdsa", Some(SignatureKind::ProviderTee)),
            ("ed25519", None),
        ] {
            let stored = attestation.get_chat_signature(&batch_chat_id, algo).await?;
            assert_eq!(stored.text, text);
            assert_eq!(stored.signature_kind, kind);
        }
    }

    Ok(())
}
