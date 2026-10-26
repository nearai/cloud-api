#[allow(dead_code)]
mod support;

use database::repositories::ApiKeyRepository;
use services::workspace::{ApiKeyId, ApiKeyRepository as ApiKeyRepositoryPort};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use support::{insert_org_fixture, test_pool};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn pool_losing_first_commit_ack() -> anyhow::Result<(
    database::DbPool,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let commits = Arc::new(AtomicUsize::new(0));
    let observed_commits = commits.clone();
    let host = std::env::var("PGHOST").unwrap_or_else(|_| "localhost".into());
    let port = std::env::var("PGPORT")
        .unwrap_or_else(|_| "5432".into())
        .parse::<u16>()?;
    let proxy = tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let host = host.clone();
            let commits = observed_commits.clone();
            tokio::spawn(async move {
                let upstream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
                let (mut client_read, mut client_write) = client.into_split();
                let (mut upstream_read, mut upstream_write) = upstream.into_split();
                tokio::select! {
                    result = tokio::io::copy(&mut client_read, &mut upstream_write) => { result?; }
                    result = async {
                        loop {
                            // PostgreSQL backend messages: tag, 4-byte length (including
                            // the length itself), then payload. Forward all other frames.
                            let tag = upstream_read.read_u8().await?;
                            let length = upstream_read.read_u32().await?;
                            anyhow::ensure!((4..=16_777_216).contains(&length), "invalid backend frame");
                            let mut payload = vec![0; (length - 4) as usize];
                            upstream_read.read_exact(&mut payload).await?;
                            if tag == b'C' && payload == b"COMMIT\0"
                                && commits.fetch_add(1, Ordering::SeqCst) == 0
                            {
                                // CommandComplete means PostgreSQL committed. Close the
                                // socket before forwarding acknowledgement to the client.
                                return Ok::<_, anyhow::Error>(());
                            }
                            client_write.write_u8(tag).await?;
                            client_write.write_u32(length).await?;
                            client_write.write_all(&payload).await?;
                        }
                    } => { result?; }
                }
                Ok::<_, anyhow::Error>(())
            });
        }
    });
    let mut config = deadpool_postgres::Config::new();
    config.host = Some(address.ip().to_string());
    config.port = Some(address.port());
    config.dbname = Some(std::env::var("PGDATABASE").unwrap_or_else(|_| "platform_api".into()));
    config.user = Some(std::env::var("PGUSER").unwrap_or_else(|_| "postgres".into()));
    config.password = Some(std::env::var("PGPASSWORD").unwrap_or_else(|_| "postgres".into()));
    let pool = database::DbPool::new(
        config.create_pool(Some(deadpool::Runtime::Tokio1), tokio_postgres::NoTls)?,
    );
    Ok((pool, commits, proxy))
}

async fn wait_for_lock_wait(pool: &database::DbPool, blocker_pid: i32) -> anyhow::Result<()> {
    let observer = pool.get().await?;
    // Prove the requests overlap at the row lock; no timing-only sleeps.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = observer.query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))",
                &[&blocker_pid],
            ).await?.get(0);
            if waiting { return Ok::<_, anyhow::Error>(()); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await?
}

#[tokio::test]
async fn updates_capture_the_transition_after_waiting_for_a_concurrent_writer() -> anyhow::Result<()>
{
    let pool = test_pool().await?;
    let repository = Arc::new(ApiKeyRepository::new(pool.clone()));
    // Exercise enable/disable, both spend-limit endpoints, and a concurrent no-op.
    for operation in 0..4 {
        let fixture = insert_org_fixture(&pool).await?;
        let id = fixture.api_key_a_id;
        let mut blocker = pool.get().await?;
        blocker
            .execute(
                "UPDATE api_keys SET spend_limit = 1000000 WHERE id = $1",
                &[&id],
            )
            .await?;
        let stale = repository.get_by_id(id).await?.unwrap();
        assert!(stale.is_active);
        assert_eq!(stale.spend_limit, Some(1_000_000));

        let blocker_pid: i32 = blocker
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let transaction = blocker.transaction().await?;
        transaction
            .execute(
                "UPDATE api_keys SET is_active = false, spend_limit = 4000000 WHERE id = $1",
                &[&id],
            )
            .await?;
        let writer_repository = repository.clone();
        let writer = tokio::spawn(async move {
            let id = ApiKeyId(id.to_string());
            if operation == 2 {
                ApiKeyRepositoryPort::update_spend_limit(
                    writer_repository.as_ref(),
                    id,
                    Some(1_000_000),
                )
                .await
            } else {
                ApiKeyRepositoryPort::update(
                    writer_repository.as_ref(),
                    id,
                    None,
                    None,
                    (operation == 1).then_some(Some(1_000_000)),
                    match operation {
                        0 => Some(true),
                        3 => Some(false),
                        _ => None,
                    },
                )
                .await
            }
        });
        wait_for_lock_wait(&pool, blocker_pid).await?;
        assert!(!writer.is_finished());
        transaction.commit().await?;
        let result = tokio::time::timeout(Duration::from_secs(5), writer).await???;
        assert!(!result.previous.is_active);
        assert_eq!(result.previous.spend_limit, Some(4_000_000));
        assert_eq!(result.updated.is_active, operation == 0);
        assert_eq!(
            result.updated.spend_limit,
            Some(if operation == 1 || operation == 2 {
                1_000_000
            } else {
                4_000_000
            })
        );
        let persisted = repository.get_by_id(id).await?.unwrap();
        assert_eq!(persisted.is_active, result.updated.is_active);
        assert_eq!(persisted.spend_limit, result.updated.spend_limit);
    }
    Ok(())
}

#[tokio::test]
async fn updates_reject_a_key_revoked_while_waiting_for_its_row_lock() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    let repository = Arc::new(ApiKeyRepository::new(pool.clone()));
    for spend_limit_only in [false, true] {
        let fixture = insert_org_fixture(&pool).await?;
        let id = fixture.api_key_a_id;
        assert!(repository.get_by_id(id).await?.is_some());
        let mut blocker = pool.get().await?;
        let blocker_pid: i32 = blocker
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let transaction = blocker.transaction().await?;
        transaction
            .execute(
                "UPDATE api_keys SET deleted_at = NOW() WHERE id = $1",
                &[&id],
            )
            .await?;
        let writer_repository = repository.clone();
        let writer = tokio::spawn(async move {
            let id = ApiKeyId(id.to_string());
            if spend_limit_only {
                ApiKeyRepositoryPort::update_spend_limit(
                    writer_repository.as_ref(),
                    id,
                    Some(1_000_000),
                )
                .await
            } else {
                ApiKeyRepositoryPort::update(
                    writer_repository.as_ref(),
                    id,
                    None,
                    None,
                    None,
                    Some(false),
                )
                .await
            }
        });
        wait_for_lock_wait(&pool, blocker_pid).await?;
        assert!(!writer.is_finished());
        transaction.commit().await?;
        let result = tokio::time::timeout(Duration::from_secs(5), writer).await??;
        assert!(matches!(
            result,
            Err(services::common::RepositoryError::NotFound(_))
        ));
        assert!(repository.get_by_id(id).await?.is_none());
        let row = blocker
            .query_one(
                "SELECT is_active, spend_limit FROM api_keys WHERE id = $1",
                &[&id],
            )
            .await?;
        assert!(row.get::<_, bool>("is_active"));
        assert_eq!(row.get::<_, Option<i64>>("spend_limit"), None);
    }
    Ok(())
}

#[tokio::test]
async fn lost_commit_acknowledgement_does_not_replay_the_update() -> anyhow::Result<()> {
    let pool = test_pool().await?;
    for spend_limit_only in [false, true] {
        let fixture = insert_org_fixture(&pool).await?;
        let id = fixture.api_key_a_id;
        let (proxy_pool, commits, proxy) = pool_losing_first_commit_ack().await?;
        let repository = ApiKeyRepository::new(proxy_pool);
        let key_id = ApiKeyId(id.to_string());
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            if spend_limit_only {
                ApiKeyRepositoryPort::update_spend_limit(&repository, key_id, Some(1_000_000)).await
            } else {
                ApiKeyRepositoryPort::update(&repository, key_id, None, None, None, Some(false))
                    .await
            }
        })
        .await?;
        proxy.abort();
        assert!(
            result.is_err(),
            "an ambiguous COMMIT must not return a replayed no-op"
        );
        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "COMMIT must not be retried"
        );
        let persisted = ApiKeyRepository::new(pool.clone())
            .get_by_id(id)
            .await?
            .unwrap();
        assert_eq!(persisted.is_active, spend_limit_only);
        assert_eq!(
            persisted.spend_limit,
            if spend_limit_only {
                Some(1_000_000)
            } else {
                None
            }
        );
    }
    Ok(())
}
