#[allow(dead_code)]
mod support;

use chrono::{Duration, Utc};
use database::repositories::SessionRepository;
use std::sync::Arc;
use support::test_pool;
use uuid::Uuid;

#[tokio::test]
async fn migration_runner_rebuilds_missing_refresh_rotation_index() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let client = pool.get().await?;
    client
        .batch_execute("DROP INDEX CONCURRENTLY IF EXISTS idx_refresh_tokens_previous_hash")
        .await?;
    drop(client);

    // Replica startup can overlap. Both runners should complete while the
    // out-of-transaction concurrent index build is serialized independently.
    let (first, second) = tokio::join!(
        database::migrations::run(&pool),
        database::migrations::run(&pool)
    );
    first?;
    second?;

    let client = pool.get().await?;
    let ready: bool = client
        .query_one(
            r#"
            SELECT index_state.indisvalid AND index_state.indisready
            FROM pg_namespace AS namespace
            JOIN pg_class AS table_class
              ON table_class.relnamespace = namespace.oid
             AND table_class.relname = 'refresh_tokens'
            JOIN pg_index AS index_state
              ON index_state.indrelid = table_class.oid
            JOIN pg_class AS index_class
              ON index_class.oid = index_state.indexrelid
             AND index_class.relnamespace = namespace.oid
             AND index_class.relname = 'idx_refresh_tokens_previous_hash'
            WHERE namespace.nspname = current_schema()
            "#,
            &[],
        )
        .await?
        .get(0);
    assert!(ready, "refresh rotation index must be valid and ready");
    Ok(())
}

#[tokio::test]
async fn concurrent_refreshes_reuse_one_successor_and_reject_stale_tokens() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let user_id = Uuid::new_v4();
    let client = pool.get().await?;
    let suffix = user_id.simple().to_string();
    client
        .execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id) VALUES ($1, $2, $3, 'test', $4)",
            &[&user_id, &format!("rotation-{suffix}@example.test"), &format!("rotation-{suffix}"), &suffix],
        )
        .await?;

    let repository = Arc::new(SessionRepository::new(pool.clone()));
    let (session, old_token) = repository
        .create(user_id, None, "rotation-test-agent".into(), 168)
        .await?;
    let successor = format!("rt_{}", Uuid::new_v4().simple());
    let mut refreshes = Vec::new();
    for _ in 0..4 {
        let repository = repository.clone();
        let old_token = old_token.clone();
        let successor = successor.clone();
        refreshes.push(tokio::spawn(async move {
            repository
                .rotate(session.id, &old_token, &successor, 168)
                .await
        }));
    }
    for refresh in refreshes {
        let (_, returned_token) = refresh.await??;
        assert_eq!(returned_token, successor);
    }
    assert!(repository
        .validate(&old_token, "rotation-test-agent")
        .await?
        .is_some());

    // A request carrying the already-current cookie must not rotate again:
    // its delayed Set-Cookie response could otherwise overwrite a newer one.
    let (_, current_result) = repository
        .rotate(session.id, &successor, "rt_unused", 168)
        .await?;
    assert_eq!(current_result, successor);

    client
        .execute(
            "UPDATE refresh_tokens SET rotated_at = $2 WHERE id = $1",
            &[&session.id, &(Utc::now() - Duration::seconds(40))],
        )
        .await?;
    let (_, delayed_result) = repository
        .rotate(session.id, &old_token, &successor, 168)
        .await?;
    assert_eq!(delayed_result, successor);

    client
        .execute(
            "UPDATE refresh_tokens SET rotated_at = $2 WHERE id = $1",
            &[&session.id, &(Utc::now() - Duration::seconds(61))],
        )
        .await?;
    assert!(repository
        .validate(&old_token, "rotation-test-agent")
        .await?
        .is_none());
    assert!(repository
        .rotate(session.id, &old_token, &successor, 168)
        .await
        .is_err());
    let (_, next_result) = repository
        .rotate(session.id, &successor, "rt_next", 168)
        .await?;
    assert_eq!(next_result, "rt_next");

    client
        .execute("DELETE FROM users WHERE id = $1", &[&user_id])
        .await?;
    Ok(())
}

#[tokio::test]
async fn mobile_os_update_accepts_a_session_with_legacy_stored_user_agent() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let client = pool.get().await?;
    let user_id = Uuid::new_v4();
    let suffix = user_id.simple().to_string();
    client
        .execute(
            "INSERT INTO users (id, email, username, auth_provider, provider_user_id) VALUES ($1, $2, $3, 'test', $4)",
            &[&user_id, &format!("ua-upgrade-{suffix}@example.test"), &format!("ua-upgrade-{suffix}"), &suffix],
        )
        .await?;

    let repository = SessionRepository::new(pool.clone());
    let (session, token) = repository
        .create(user_id, None, "legacy mobile agent".into(), 168)
        .await?;
    // This is what the previous normalization stored: browser versions were
    // removed, but the iOS version remained in the session row.
    let legacy_user_agent =
        "Mozilla (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit Version Mobile Safari";
    client
        .execute(
            "UPDATE refresh_tokens SET user_agent = $2 WHERE id = $1",
            &[&session.id, &legacy_user_agent],
        )
        .await?;

    let updated_user_agent = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 Version/18.0 Mobile/15E148 Safari/604.1";
    assert!(repository
        .validate(&token, updated_user_agent)
        .await?
        .is_some());

    client
        .execute("DELETE FROM users WHERE id = $1", &[&user_id])
        .await?;
    Ok(())
}
