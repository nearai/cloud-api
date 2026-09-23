use super::*;

// Uses unique keys and expires/deletes only those keys. Never flushes Redis.
#[tokio::test]
#[ignore = "requires REDIS_ADMISSION_TEST_URL on an explicitly provisioned Redis instance"]
async fn redis_rejects_reordered_and_expired_fills_without_renewing_ttl() -> anyhow::Result<()> {
    let config = config::AdmissionCacheConfig {
        redis_url: Some(std::env::var("REDIS_ADMISSION_TEST_URL")?),
        ttl_seconds: 30,
        ttl_jitter_seconds: 0,
        command_deadline_ms: 2_000,
        fallback_concurrency: 1,
        fallback_deadline_ms: 2_000,
        max_fill_age_ms: 1_000,
    };
    let cache = RedisAdmissionCache::new(&config)?;
    let org = Uuid::new_v4();
    let key = RedisAdmissionCache::key_org(org);
    let snapshot = OrganizationAdmissionSnapshot {
        organization_id: org,
        revision: 9_007_199_254_740_993,
        total_spent: Some(i64::MAX),
        limit: None,
    };
    let started = cache.server_time_ms().await?;
    assert!(cache.put_organization(&snapshot, started).await?);
    let mut connection = cache.connection().await?;
    let ttl_before: i64 = redis::cmd("PTTL")
        .arg(&key)
        .query_async(&mut connection)
        .await?;
    assert!((1..=30_000).contains(&ttl_before));
    let cached = cache.get_organization(org).await?.expect("cache filled");
    assert_eq!(cached.revision, snapshot.revision);
    assert_eq!(cached.total_spent, Some(i64::MAX));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let stale = OrganizationAdmissionSnapshot {
        revision: snapshot.revision - 1,
        total_spent: Some(1),
        ..snapshot.clone()
    };
    let fresh_time = cache.server_time_ms().await?;
    assert!(!cache.put_organization(&stale, fresh_time).await?);
    let ttl_after: i64 = redis::cmd("PTTL")
        .arg(&key)
        .query_async(&mut connection)
        .await?;
    assert!(
        ttl_after < ttl_before,
        "reads/rejected refresh must not renew TTL"
    );
    assert_eq!(
        cache.get_organization(org).await?.unwrap().revision,
        snapshot.revision
    );

    // A genuinely fresh read at the same revision renews lifetime.
    assert!(
        cache
            .put_organization(&snapshot, cache.server_time_ms().await?)
            .await?
    );
    let renewed: i64 = redis::cmd("PTTL")
        .arg(&key)
        .query_async(&mut connection)
        .await?;
    assert!(renewed > ttl_after);
    // Replaying the same old command cannot grant a new lifetime.
    assert!(cache.put_organization(&snapshot, started).await?);
    let replayed: i64 = redis::cmd("PTTL")
        .arg(&key)
        .query_async(&mut connection)
        .await?;
    assert!(replayed <= ttl_after);

    let _: i64 = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut connection)
        .await?;
    let now = cache.server_time_ms().await?;
    assert!(!cache.put_organization(&snapshot, now - 1_001).await?);
    assert!(!cache.put_organization(&snapshot, now + 60_000).await?);
    assert!(cache.get_organization(org).await?.is_none());
    Ok(())
}
