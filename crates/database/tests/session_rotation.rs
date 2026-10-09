#[allow(dead_code)]
mod support;

use database::repositories::SessionRepository;
use std::sync::Arc;
use support::test_pool;
use uuid::Uuid;

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
            "UPDATE refresh_tokens SET rotated_at = NOW() - INTERVAL '40 seconds' WHERE id = $1",
            &[&session.id],
        )
        .await?;
    let (_, delayed_result) = repository
        .rotate(session.id, &old_token, &successor, 168)
        .await?;
    assert_eq!(delayed_result, successor);

    client
        .execute(
            "UPDATE refresh_tokens SET rotated_at = NOW() - INTERVAL '61 seconds' WHERE id = $1",
            &[&session.id],
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
