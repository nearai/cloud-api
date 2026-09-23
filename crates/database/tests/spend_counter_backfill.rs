use chrono::Utc;
use database::models::RecordUsageRequest;
use database::repositories::{
    OrganizationServiceUsageRepository, OrganizationUsageRepository, RecordServiceUsageRequest,
};
use database::{ensure_spend_counters_ready, migrations, DbPool, PreparedSpendBackfill};
use deadpool::Runtime;
use deadpool_postgres::{Config, PoolConfig, Timeouts};
use services::usage::InferenceType;
use std::future::Future;
use std::time::Duration;
use tokio_postgres::NoTls;
use uuid::Uuid;

#[path = "spend_counter_backfill/coverage.rs"]
mod coverage;

struct TestDatabase {
    pool: DbPool,
    admin: DbPool,
    database_name: String,
}

struct Fixture {
    organization_id: Uuid,
    mixed_key: Uuid,
    service_key: Uuid,
    deleted_key: Uuid,
    empty_key: Uuid,
    model_id: Uuid,
    service_id: Uuid,
    workspace_id: Uuid,
}

#[tokio::test]
async fn backfill_reconciles_snapshot_delta_and_is_idempotent() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
    let fixture = fixture(&database.pool).await?;

    // Historical rows exceed the C1 counters already present for the mixed key.
    insert_inference(&database.pool, &fixture, fixture.mixed_key, 100).await?;
    insert_service(&database.pool, &fixture, fixture.mixed_key, 40).await?;
    insert_service(&database.pool, &fixture, fixture.service_key, 60).await?;
    insert_service(&database.pool, &fixture, fixture.deleted_key, 10).await?;
    let client = database.pool.get().await?;
    client
        .execute(
            "INSERT INTO api_key_spend (api_key_id, inference_spent, service_spent)
             VALUES ($1, 20, 10)",
            &[&fixture.mixed_key],
        )
        .await?;
    client
        .execute(
            "UPDATE organization_balance SET total_spent = 777, inference_spent = 20, service_spent = 10,
             spend_counters_ready_at = NULL WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await?;
    drop(client);

    // Prepare twice from the same snapshot, then post through C1 before applying.
    let first = PreparedSpendBackfill::prepare(
        &database.pool,
        fixture.organization_id,
        Duration::from_secs(30),
    )
    .await?
    .expect("fixture organization is incomplete");
    let second = PreparedSpendBackfill::prepare(
        &database.pool,
        fixture.organization_id,
        Duration::from_secs(30),
    )
    .await?
    .expect("second snapshot is also incomplete");
    let inference_repository = OrganizationUsageRepository::new(database.pool.clone());
    let service_repository = OrganizationServiceUsageRepository::new(database.pool.clone());
    inference_repository
        .record_usage(inference_request(&fixture, 5))
        .await?;
    service_repository
        .record_usage(&RecordServiceUsageRequest {
            organization_id: fixture.organization_id,
            workspace_id: fixture.workspace_id,
            api_key_id: fixture.mixed_key,
            service_id: fixture.service_id,
            quantity: 1,
            total_cost: 7,
            inference_id: Some(Uuid::new_v4()),
        })
        .await?;

    let (first_result, second_result) = tokio::join!(first.apply(), second.apply());
    let outcomes = [first_result?, second_result?];
    assert!(outcomes.iter().any(|outcome| matches!(
        outcome,
        database::SpendBackfillOutcome::Applied { key_count: 3 }
    )));
    assert!(outcomes.contains(&database::SpendBackfillOutcome::AlreadyComplete));
    assert!(PreparedSpendBackfill::prepare(
        &database.pool,
        fixture.organization_id,
        Duration::from_secs(30)
    )
    .await?
    .is_none());

    let client = database.pool.get().await?;
    let row = client
        .query_one(
            "SELECT total_spent, inference_spent, service_spent, spend_counters_ready_at
             FROM organization_balance WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await?;
    assert_eq!(row.get::<_, i64>("total_spent"), 789);
    assert_eq!(row.get::<_, i64>("inference_spent"), 105);
    assert_eq!(row.get::<_, i64>("service_spent"), 117);
    assert!(row
        .get::<_, Option<chrono::DateTime<Utc>>>("spend_counters_ready_at")
        .is_some());
    let rows = client
        .query(
            "SELECT api_key_id, inference_spent, service_spent FROM api_key_spend
             WHERE api_key_id = ANY($1) ORDER BY api_key_id",
            &[&vec![
                fixture.mixed_key,
                fixture.service_key,
                fixture.deleted_key,
            ]],
        )
        .await?;
    assert_eq!(rows.len(), 3);
    let totals: Vec<(Uuid, i64, i64)> = rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    assert!(totals.contains(&(fixture.mixed_key, 105, 47)));
    assert!(totals.contains(&(fixture.service_key, 0, 60)));
    assert!(totals.contains(&(fixture.deleted_key, 0, 10)));
    assert_eq!(
        client
            .query_one(
                "SELECT inference_spent, service_spent FROM api_key_spend WHERE api_key_id = $1",
                &[&fixture.mixed_key],
            )
            .await?
            .get::<_, i64>(0),
        105
    );
    drop(client);
    drop(inference_repository);
    drop(service_repository);
    Ok(())
    })
    .await
}

#[tokio::test]
async fn backfill_rejects_negative_drift_and_rolls_back_overflow() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
    let fixture = fixture(&database.pool).await?;
    let client = database.pool.get().await?;
    client
        .execute(
            "INSERT INTO api_key_spend (api_key_id, inference_spent)
             VALUES ($1, 1)",
            &[&fixture.empty_key],
        )
        .await?;
    client
        .execute(
            "UPDATE organization_balance SET spend_counters_ready_at = NULL
             WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await?;
    drop(client);
    let error = PreparedSpendBackfill::prepare(
        &database.pool,
        fixture.organization_id,
        Duration::from_secs(30),
    )
    .await
    .expect_err("counter-only spend must reject negative correction");
    assert!(error.to_string().contains("counter exceeds raw history"));

    let client = database.pool.get().await?;
    client
        .execute(
            "DELETE FROM api_key_spend WHERE api_key_id = $1",
            &[&fixture.empty_key],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, total_tokens, input_cost, output_cost,
                total_cost, inference_type, inference_id
             ) VALUES ($1, $2, $3, $4, $5, 'counter-test', 1, 0, 1, 1, 0, 1,
                       'chat_completion', $1)",
            &[
                &Uuid::new_v4(),
                &fixture.organization_id,
                &fixture.workspace_id,
                &fixture.empty_key,
                &fixture.model_id,
            ],
        )
        .await?;
    client
        .execute(
            "UPDATE organization_balance SET spend_counters_ready_at = NULL
             WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await?;
    drop(client);
    let prepared = PreparedSpendBackfill::prepare(
        &database.pool,
        fixture.organization_id,
        Duration::from_secs(30),
    )
    .await?
    .expect("overflow fixture is incomplete");
    let client = database.pool.get().await?;
    client
        .execute(
            "INSERT INTO api_key_spend (api_key_id, inference_spent)
             VALUES ($1, $2)",
            &[&fixture.empty_key, &i64::MAX],
        )
        .await?;
    drop(client);
    let error = prepared
        .apply()
        .await
        .expect_err("counter overflow must fail during the apply transaction");
    let pg_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
        .expect("overflow should preserve the PostgreSQL error cause");
    assert_eq!(
        pg_error.code(),
        Some(&tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
    );
    let client = database.pool.get().await?;
    let row = client
        .query_one(
            "SELECT inference_spent, spend_counters_ready_at
             FROM organization_balance WHERE organization_id = $1",
            &[&fixture.organization_id],
        )
        .await?;
    assert!(row.get::<_, Option<chrono::DateTime<Utc>>>(1).is_none());
    assert_eq!(
        client
            .query_one(
                "SELECT inference_spent FROM api_key_spend WHERE api_key_id = $1",
                &[&fixture.empty_key],
            )
            .await?
            .get::<_, i64>(0),
        i64::MAX
    );
    assert_eq!(row.get::<_, i64>(0), 0);
    client
        .execute(
            "DELETE FROM api_key_spend WHERE api_key_id = $1",
            &[&fixture.empty_key],
        )
        .await?;
    drop(client);

    // Fail after the key upsert to prove the entire transaction rolls back.
    let prepared = PreparedSpendBackfill::prepare(
        &database.pool,
        fixture.organization_id,
        Duration::from_secs(30),
    )
    .await?
    .expect("failed apply must leave the organization retryable");
    let client = database.pool.get().await?;
    client
        .execute(
            "UPDATE organization_balance SET inference_spent = $2 WHERE organization_id = $1",
            &[&fixture.organization_id, &i64::MAX],
        )
        .await?;
    drop(client);
    let error = prepared
        .apply()
        .await
        .expect_err("balance overflow must roll back key corrections");
    let database_error = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<tokio_postgres::Error>())
        .expect("preserved PostgreSQL error");
    assert_eq!(
        database_error.code(),
        Some(&tokio_postgres::error::SqlState::NUMERIC_VALUE_OUT_OF_RANGE)
    );
    let client = database.pool.get().await?;
    assert!(client
        .query_opt(
            "SELECT 1 FROM api_key_spend WHERE api_key_id = $1",
            &[&fixture.empty_key]
        )
        .await?
        .is_none());
    let balance = client.query_one(
        "SELECT inference_spent, spend_counters_ready_at FROM organization_balance WHERE organization_id = $1",
        &[&fixture.organization_id],
    ).await?;
    assert_eq!(balance.get::<_, i64>(0), i64::MAX);
    assert!(balance.get::<_, Option<chrono::DateTime<Utc>>>(1).is_none());
    drop(client);
    Ok(())
    })
    .await
}

#[tokio::test]
async fn spend_readiness_checks_missing_and_inactive_organizations() -> anyhow::Result<()> {
    run_backfill_test(|database| async move {
        let fixture = fixture(&database.pool).await?;
        let client = database.pool.get().await?;
        client
            .execute(
                "UPDATE organization_balance SET spend_counters_ready_at = NULL
             WHERE organization_id = $1",
                &[&fixture.organization_id],
            )
            .await?;
        drop(client);
        let prepared = PreparedSpendBackfill::prepare(
            &database.pool,
            fixture.organization_id,
            Duration::from_secs(30),
        )
        .await?
        .expect("empty historical organization is incomplete");
        assert_eq!(
            prepared.apply().await?,
            database::SpendBackfillOutcome::Applied { key_count: 0 }
        );
        ensure_spend_counters_ready(&database.pool).await?;

        let mut client = database.pool.get().await?;
        let migration_tx = client.build_transaction().start().await?;
        migration_tx
            .batch_execute(
                "CREATE TEMP TABLE organization_balance (
                 organization_id UUID PRIMARY KEY,
                 inference_spent BIGINT NOT NULL DEFAULT 0,
                 service_spent BIGINT NOT NULL DEFAULT 0
             );
             INSERT INTO organization_balance (organization_id) VALUES (uuid_generate_v4());",
            )
            .await?;
        migration_tx
            .batch_execute(include_str!(
                "../src/migrations/sql/V0082__add_spend_counters_readiness.sql"
            ))
            .await?;
        let legacy = migration_tx
            .query_one(
                "SELECT spend_counters_ready_at FROM organization_balance LIMIT 1",
                &[],
            )
            .await?;
        assert!(legacy.get::<_, Option<chrono::DateTime<Utc>>>(0).is_none());
        let future = migration_tx
            .query_one(
                "INSERT INTO organization_balance (organization_id)
             VALUES (uuid_generate_v4())
             RETURNING spend_counters_ready_at",
                &[],
            )
            .await?;
        assert!(future.get::<_, Option<chrono::DateTime<Utc>>>(0).is_some());
        migration_tx.rollback().await?;

        client
            .execute(
                "UPDATE organizations SET is_active = false WHERE id = $1",
                &[&fixture.organization_id],
            )
            .await?;
        ensure_spend_counters_ready(&database.pool).await?;
        client
            .execute(
                "UPDATE organization_balance SET spend_counters_ready_at = NULL
             WHERE organization_id = $1",
                &[&fixture.organization_id],
            )
            .await?;
        assert!(ensure_spend_counters_ready(&database.pool)
            .await
            .expect_err("inactive incomplete organizations must block readers")
            .to_string()
            .contains("remain unreconciled"));
        client
            .execute(
                "DELETE FROM organization_balance WHERE organization_id = $1",
                &[&fixture.organization_id],
            )
            .await?;
        assert!(ensure_spend_counters_ready(&database.pool)
            .await
            .expect_err("missing balances must be distinguished")
            .to_string()
            .contains("lack balances"));
        drop(client);
        Ok(())
    })
    .await
}

async fn run_backfill_test<F, Fut>(test: F) -> anyhow::Result<()>
where
    F: FnOnce(TestDatabase) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    run_backfill_test_at(None, test).await
}

/// Like `run_backfill_test`, but stops migrating at `schema_version` when given.
async fn run_backfill_test_at<F, Fut>(schema_version: Option<i32>, test: F) -> anyhow::Result<()>
where
    F: FnOnce(TestDatabase) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let database = test_database(schema_version).await?;
    let admin = database.admin.clone();
    let database_name = database.database_name.clone();
    let result = tokio::spawn(test(database)).await;
    let cleanup: anyhow::Result<()> = async {
        let client = admin.get().await?;
        client
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                database_name
            ))
            .await?;
        Ok(())
    }
    .await;
    let test_result = match result {
        Ok(result) => result,
        Err(error) => Err(anyhow::Error::new(error).context("backfill test task failed")),
    };
    test_result?;
    cleanup?;
    Ok(())
}

async fn test_database(schema_version: Option<i32>) -> anyhow::Result<TestDatabase> {
    let admin_config = pool_config(None);
    let admin: DbPool = admin_config
        .create_pool(Some(Runtime::Tokio1), NoTls)?
        .into();
    let database_name = format!("spend_counter_backfill_{}", Uuid::new_v4().simple());
    admin
        .get()
        .await?
        .batch_execute(&format!("CREATE DATABASE {database_name}"))
        .await?;
    let scoped_config = pool_config(Some(&database_name));
    let pool: DbPool = scoped_config
        .create_pool(Some(Runtime::Tokio1), NoTls)?
        .into();
    match schema_version {
        None => migrations::run(&pool).await?,
        Some(version) => {
            let sql = concat!(env!("CARGO_MANIFEST_DIR"), "/src/migrations/sql");
            let migrations = refinery::load_sql_migrations(sql)?;
            let mut client = pool.get().await?;
            refinery::Runner::new(&migrations)
                .set_target(refinery::Target::Version(version))
                .run_async(&mut **client)
                .await?;
        }
    }
    Ok(TestDatabase {
        pool,
        admin,
        database_name,
    })
}

fn pool_config(database_name: Option<&str>) -> Config {
    let mut config = Config::new();
    config.host = Some(
        std::env::var("PGHOST")
            .or_else(|_| std::env::var("DATABASE_HOST"))
            .unwrap_or_else(|_| "localhost".to_string()),
    );
    config.port = Some(
        std::env::var("PGPORT")
            .or_else(|_| std::env::var("DATABASE_PORT"))
            .unwrap_or_else(|_| "5432".to_string())
            .parse()
            .expect("database port must be numeric"),
    );
    config.dbname = Some(database_name.map(ToString::to_string).unwrap_or_else(|| {
        std::env::var("PGDATABASE")
            .or_else(|_| std::env::var("DATABASE_NAME"))
            .unwrap_or_else(|_| "platform_api".to_string())
    }));
    config.user = Some(
        std::env::var("PGUSER")
            .or_else(|_| std::env::var("DATABASE_USERNAME"))
            .unwrap_or_else(|_| "postgres".to_string()),
    );
    config.password = Some(
        std::env::var("PGPASSWORD")
            .or_else(|_| std::env::var("DATABASE_PASSWORD"))
            .unwrap_or_else(|_| "postgres".to_string()),
    );
    config.pool = Some(PoolConfig {
        max_size: 4,
        timeouts: Timeouts {
            wait: Some(Duration::from_secs(10)),
            create: Some(Duration::from_secs(10)),
            recycle: Some(Duration::from_secs(10)),
        },
        ..Default::default()
    });
    config
}

async fn fixture(pool: &DbPool) -> anyhow::Result<Fixture> {
    let client = pool.get().await?;
    let now = Utc::now();
    let suffix = Uuid::new_v4().simple().to_string();
    let user_id = Uuid::new_v4();
    let organization_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let mixed_key = Uuid::new_v4();
    let service_key = Uuid::new_v4();
    let deleted_key = Uuid::new_v4();
    let empty_key = Uuid::new_v4();
    let model_id = Uuid::new_v4();
    let service_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id)
             VALUES ($1, $2, $3, 'test', $4)",
            &[
                &user_id,
                &format!("{suffix}@example.test"),
                &suffix,
                &format!("provider-{suffix}"),
            ],
        )
        .await?;
    client
        .execute(
            "INSERT INTO organizations (id, name, created_at, updated_at)
             VALUES ($1, $2, $3, $3)",
            &[&organization_id, &format!("org-{suffix}"), &now],
        )
        .await?;
    client
        .execute(
            "INSERT INTO workspaces (id, name, organization_id, created_by_user_id)
             VALUES ($1, 'workspace', $2, $3)",
            &[&workspace_id, &organization_id, &user_id],
        )
        .await?;
    for (id, name) in [
        (mixed_key, "mixed"),
        (service_key, "service"),
        (deleted_key, "deleted"),
        (empty_key, "empty"),
    ] {
        client
            .execute(
                "INSERT INTO api_keys
                 (id, key_hash, key_prefix, name, workspace_id, created_by_user_id)
                 VALUES ($1, $2, 'test', $3, $4, $5)",
                &[&id, &format!("hash-{id}"), &name, &workspace_id, &user_id],
            )
            .await?;
    }
    client
        .execute(
            "UPDATE api_keys SET deleted_at = NOW(), is_active = false WHERE id = $1",
            &[&deleted_key],
        )
        .await?;
    client
        .execute(
            "INSERT INTO models (id, model_name, model_display_name, model_description)
             VALUES ($1, 'counter-test', 'counter-test', 'counter-test')",
            &[&model_id],
        )
        .await?;
    client
        .execute(
            "INSERT INTO services
             (id, service_name, display_name, unit, cost_per_unit)
             VALUES ($1, $2, $2, 'request', 1)",
            &[&service_id, &format!("service-{suffix}")],
        )
        .await?;
    Ok(Fixture {
        organization_id,
        mixed_key,
        service_key,
        deleted_key,
        empty_key,
        model_id,
        service_id,
        workspace_id,
    })
}

fn inference_request(fixture: &Fixture, total_cost: i64) -> RecordUsageRequest {
    RecordUsageRequest {
        organization_id: fixture.organization_id,
        workspace_id: fixture.workspace_id,
        api_key_id: fixture.mixed_key,
        model_id: fixture.model_id,
        model_name: "counter-test".to_string(),
        input_tokens: 1,
        output_tokens: 1,
        input_cost: total_cost,
        output_cost: 0,
        total_cost,
        inference_type: InferenceType::ChatCompletion.as_str().to_string(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: Some(format!("counter-{total_cost}-{}", Uuid::new_v4())),
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

async fn insert_inference(
    pool: &DbPool,
    fixture: &Fixture,
    api_key_id: Uuid,
    cost: i64,
) -> anyhow::Result<()> {
    let client = pool.get().await?;
    let id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO organization_usage_log (
                id, organization_id, workspace_id, api_key_id, model_id, model_name,
                input_tokens, output_tokens, total_tokens, input_cost, output_cost,
                total_cost, inference_type, inference_id
             ) VALUES ($1, $2, $3, $4, $5, 'counter-test', 1, 0, 1, $6, 0, $6,
                       'chat_completion', $1)",
            &[
                &id,
                &fixture.organization_id,
                &fixture.workspace_id,
                &api_key_id,
                &fixture.model_id,
                &cost,
            ],
        )
        .await?;
    Ok(())
}

async fn insert_service(
    pool: &DbPool,
    fixture: &Fixture,
    api_key_id: Uuid,
    cost: i64,
) -> anyhow::Result<()> {
    let client = pool.get().await?;
    client
        .execute(
            "INSERT INTO organization_service_usage_log
             (organization_id, workspace_id, api_key_id, service_id, quantity, total_cost)
             VALUES ($1, $2, $3, $4, 1, $5)",
            &[
                &fixture.organization_id,
                &fixture.workspace_id,
                &api_key_id,
                &fixture.service_id,
                &cost,
            ],
        )
        .await?;
    Ok(())
}
