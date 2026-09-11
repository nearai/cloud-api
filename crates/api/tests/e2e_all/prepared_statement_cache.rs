//! Hot-path queries run through the connection's prepared-statement cache.
//! During a rolling deploy a replica on the previous release keeps serving
//! after the new release has run its migrations; if one of those migrations
//! adds a column to a table read with `SELECT *`, Postgres refuses the old
//! plan with "cached plan must not change result type". The repositories must
//! recover from that transparently, not surface a 500.

use crate::common::*;
use futures::future::join_all;

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
