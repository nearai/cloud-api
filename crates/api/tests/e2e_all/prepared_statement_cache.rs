//! Hot-path queries run through the connection's prepared-statement cache.
//! During a rolling deploy a replica on the previous release keeps serving
//! after the new release has run its migrations; if one of those migrations
//! adds a column to a table read with `SELECT *`, Postgres refuses the old
//! plan with "cached plan must not change result type". The repositories must
//! recover from that transparently, not surface a 500.

use crate::common::*;
use anyhow::{ensure, Context, Result};
use database::{models::RecordUsageRequest, repositories::OrganizationUsageRepository};
use futures::future::join_all;
use uuid::Uuid;

#[tokio::test]
async fn cached_statements_recover_from_a_column_added_under_them() {
    let (server, database) = setup_test_server_with_database().await;
    let (session_id, _email) = setup_unique_test_session(&database).await;
    let org = create_org_with_session(&server, &session_id).await;
    let api_key = get_api_key_for_org_with_session(&server, org.id.clone(), &session_id).await;

    let probe = || async {
        server
            .get("/v1/files?limit=1")
            .add_header("Authorization", format!("Bearer {api_key}"))
            .add_header("User-Agent", MOCK_USER_AGENT)
            .await
    };

    // Warm the cache: API-key auth resolves the workspace and organization
    // with a `SELECT w.*` on every request. Probes run concurrently so the
    // statement is cached on every connection of the test pool, not just
    // one; that is the shape of a warm production replica.
    for response in join_all((0..8).map(|_| probe())).await {
        assert_eq!(response.status_code(), 200, "{}", response.text());
    }

    // A migration on the next release adds a column to `workspaces` while
    // this process still holds statements prepared against the old shape.
    let column = format!(
        "rolling_deploy_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    {
        let client = database
            .pool()
            .get()
            .await
            .expect("failed to get database connection");
        client
            .execute(
                &format!("ALTER TABLE workspaces ADD COLUMN {column} TEXT"),
                &[],
            )
            .await
            .expect("failed to add column");
    }

    // Every request must still succeed: the stale plan is dropped from the
    // cache and the statement re-prepared inside the repository retry.
    // Collect instead of asserting so the column is dropped even on failure;
    // a leftover column would fail the database-encryption classification
    // scans that share this database.
    // Concurrent again: every warm connection holds a stale plan, and each
    // request must recover on whichever connection it lands on.
    let outcomes: Vec<_> = join_all((0..8).map(|_| probe()))
        .await
        .into_iter()
        .map(|response| (response.status_code(), response.text()))
        .collect();

    // Best-effort cleanup that never panics: a leftover column breaks the
    // database-encryption classification scans that share this database, so
    // the drop must run even when the pool or the statement misbehaves, and
    // the assertions below must still report the real outcome.
    let cleanup = async {
        let client = database.pool().get().await?;
        client
            .execute(
                &format!("ALTER TABLE workspaces DROP COLUMN IF EXISTS {column}"),
                &[],
            )
            .await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    let cleanup = cleanup.await;

    for (status, body) in outcomes {
        assert_eq!(
            status, 200,
            "request after a schema change must recover, got: {body}"
        );
    }

    cleanup.expect("failed to drop the temporary column");
}

enum UsageStatementInvalidation {
    AddedColumn,
    Deallocated,
}

async fn usage_fixture() -> (database::pool::DbPool, RecordUsageRequest) {
    let pool = db_setup::create_test_pool().await;
    let client = pool.get().await.expect("fixture connection");
    let suffix = Uuid::new_v4().to_string();
    let user_id = Uuid::parse_str(MOCK_USER_ID).unwrap();
    let row = client
        .query_one(
            "WITH org AS (
                INSERT INTO organizations (name) VALUES ($1) RETURNING id
             ), workspace AS (
                INSERT INTO workspaces (name, organization_id, created_by_user_id)
                SELECT $1, org.id, $2 FROM org RETURNING id
             ), api_key AS (
                INSERT INTO api_keys (key_hash, key_prefix, name, workspace_id, created_by_user_id)
                SELECT lpad(replace(workspace.id::text, '-', ''), 64, '0'), 'sk-test', $1, workspace.id, $2
                FROM workspace RETURNING id
             )
             SELECT org.id AS organization_id, workspace.id AS workspace_id,
                    api_key.id AS api_key_id, models.id AS model_id
             FROM org, workspace, api_key, models WHERE models.model_name = $3",
            &[&suffix, &user_id, &E2E_QWEN_MODEL_NAME],
        )
        .await
        .expect("test-owned usage fixture");
    let request = RecordUsageRequest {
        organization_id: row.get("organization_id"),
        workspace_id: row.get("workspace_id"),
        api_key_id: row.get("api_key_id"),
        model_id: row.get("model_id"),
        model_name: E2E_QWEN_MODEL_NAME.to_string(),
        input_tokens: 12,
        output_tokens: 8,
        input_cost: 12_000_000,
        output_cost: 16_000_000,
        total_cost: 28_000_000,
        inference_type: services::usage::InferenceType::ChatCompletion
            .as_str()
            .to_string(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: Some(Uuid::new_v4()),
        provider_request_id: None,
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
    drop(client);
    (pool, request)
}

async fn assert_usage_recovers_without_double_charging(invalidation: UsageStatementInvalidation) {
    let (pool, request) = usage_fixture().await;
    let repository = OrganizationUsageRepository::new(pool.clone());
    let pool_size = pool.status().expect("active pool").max_size;
    // More warm connections than retry_db!'s three attempts proves that
    // recovery cannot rely on eventually acquiring a cold connection.
    assert!(pool_size > 3);
    let mut connections = Vec::new();
    for _ in 0..pool_size {
        connections.push(pool.get().await.expect("reserve pool connection"));
    }
    for _ in 0..pool_size {
        // Only this connection is available to the real repository. Retain
        // it after warming, then release the next unvisited connection.
        drop(connections.remove(0));
        let mut warm_request = request.clone();
        warm_request.inference_id = Some(Uuid::new_v4());
        assert!(
            repository
                .record_usage(warm_request)
                .await
                .unwrap()
                .was_inserted
        );
        let client = pool.get().await.expect("reacquire warmed connection");
        let cached_insert: bool = client
            .query_one(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_prepared_statements
                    WHERE statement LIKE '%INSERT INTO organization_usage_log%'
                      AND statement NOT LIKE '%pg_prepared_statements%'
                 )",
                &[],
            )
            .await
            .expect("inspect warmed session")
            .get(0);
        assert!(
            cached_insert,
            "every session must hold the real usage INSERT"
        );
        connections.push(client);
    }

    let column = format!("usage_cache_{}", Uuid::new_v4().simple());
    match invalidation {
        UsageStatementInvalidation::AddedColumn => {
            connections[0]
                .batch_execute(&format!(
                    "ALTER TABLE organization_usage_log ADD COLUMN {column} TEXT"
                ))
                .await
                .expect("invalidate the cached INSERT RETURNING result type");
        }
        UsageStatementInvalidation::Deallocated => {
            for client in &connections {
                // Invalidate server handles while retaining deadpool's cache.
                client.batch_execute("DEALLOCATE ALL").await.unwrap();
            }
        }
    }
    drop(connections);

    // Return errors instead of panicking until after DDL cleanup, so a failed
    // regression does not leave an unclassified column for encryption tests.
    let outcome = async {
        let recovered = repository
            .record_usage(request.clone())
            .await
            .context("record usage after invalidating every warm connection")?;
        ensure!(recovered.was_inserted, "recovered usage must be inserted");
        let mut reserved = Vec::new();
        for _ in 1..pool_size {
            reserved.push(pool.get().await?);
        }
        let mut duplicate_request = request.clone();
        duplicate_request.total_cost *= 2;
        let duplicate = repository.record_usage(duplicate_request).await?;
        ensure!(duplicate.id == recovered.id && !duplicate.was_inserted);
        ensure!(duplicate.total_cost == request.total_cost);

        // Only one connection is free: the new write must reuse the session
        // (and cached INSERT) whose duplicate transaction just rolled back.
        let mut after_rollback = request.clone();
        after_rollback.inference_id = Some(Uuid::new_v4());
        ensure!(repository.record_usage(after_rollback).await?.was_inserted);

        let expected_requests = pool_size as i64 + 2;
        let expected_spend = expected_requests * request.total_cost;
        let expected_tokens =
            expected_requests * i64::from(request.input_tokens + request.output_tokens);
        let client = pool.get().await?;
        let usage = client
            .query_one(
                "SELECT COUNT(*)::BIGINT, SUM(total_cost)::BIGINT, SUM(total_tokens)::BIGINT,
                        COUNT(*) FILTER (WHERE inference_id = $2)::BIGINT
                 FROM organization_usage_log WHERE organization_id = $1",
                &[&request.organization_id, &request.inference_id],
            )
            .await?;
        ensure!(
            usage.get::<_, i64>(0) == expected_requests,
            "usage row count"
        );
        ensure!(usage.get::<_, i64>(1) == expected_spend, "usage cost");
        ensure!(usage.get::<_, i64>(2) == expected_tokens, "usage tokens");
        ensure!(
            usage.get::<_, i64>(3) == 1,
            "recovered inference appears once"
        );
        drop(client);
        let balance = repository
            .get_balance(request.organization_id)
            .await?
            .context("organization balance must exist")?;
        ensure!(
            balance.total_requests == expected_requests,
            "balance requests"
        );
        ensure!(
            balance.total_spent == expected_spend,
            "balance charged exactly once"
        );
        ensure!(balance.total_tokens == expected_tokens, "balance tokens");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    let cleanup: Result<()> = async {
        if matches!(invalidation, UsageStatementInvalidation::AddedColumn) {
            pool.get()
                .await?
                .batch_execute(&format!(
                    "ALTER TABLE organization_usage_log DROP COLUMN IF EXISTS {column}"
                ))
                .await?;
        }
        Ok(())
    }
    .await;
    outcome.expect("usage transaction must recover without duplicate billing");
    cleanup.expect("remove temporary usage column");
}

#[tokio::test]
async fn usage_transaction_recovers_from_changed_result_type_without_double_charging() {
    assert_usage_recovers_without_double_charging(UsageStatementInvalidation::AddedColumn).await;
}

#[tokio::test]
async fn usage_transaction_recovers_from_missing_statements_without_double_charging() {
    assert_usage_recovers_without_double_charging(UsageStatementInvalidation::Deallocated).await;
}
