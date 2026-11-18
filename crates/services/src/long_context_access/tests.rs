use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use uuid::Uuid;

use super::{LongContextAccess, LongContextAccessRepository};

const MODEL: &str = "org/model";
const OTHER: &str = "org/other";

/// Repository fake: answers `blocked` and `settings`, or fails while the
/// matching flag is set; counts block reads.
#[derive(Default)]
struct Fake {
    blocked: Mutex<Vec<String>>,
    settings: Mutex<Option<serde_json::Value>>,
    blocks_fail: AtomicBool,
    settings_fail: AtomicBool,
    blocks_delay: Option<Duration>,
    block_reads: AtomicUsize,
}

impl Fake {
    fn new(blocked: &[&str], settings: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            blocked: Mutex::new(blocked.iter().map(|name| name.to_string()).collect()),
            settings: Mutex::new(Some(settings)),
            ..Default::default()
        })
    }
}

#[async_trait]
impl LongContextAccessRepository for Fake {
    async fn blocked_model_names(&self, _: Uuid) -> anyhow::Result<Vec<String>> {
        self.block_reads.fetch_add(1, Ordering::SeqCst);
        if let Some(delay) = self.blocks_delay {
            tokio::time::sleep(delay).await;
        }
        if self.blocks_fail.load(Ordering::SeqCst) {
            anyhow::bail!("read failed");
        }
        Ok(self.blocked.lock().unwrap().clone())
    }

    async fn organization_settings(&self, _: Uuid) -> anyhow::Result<Option<serde_json::Value>> {
        if self.settings_fail.load(Ordering::SeqCst) {
            anyhow::bail!("read failed");
        }
        Ok(self.settings.lock().unwrap().clone())
    }
}

fn access(repository: Arc<Fake>, ttl: Duration) -> LongContextAccess {
    // The short cold-fallback TTL is long here so only the tests about it
    // see it expire.
    LongContextAccess::with_timing(repository, ttl, ttl, Duration::from_millis(200))
}

/// Long enough for a 50 ms policy entry to expire.
async fn expire() {
    tokio::time::sleep(Duration::from_millis(120)).await;
}

#[tokio::test]
async fn blocks_and_the_organization_switch_both_turn_the_tier_off() {
    let org = Uuid::new_v4();
    let ttl = Duration::from_secs(30);

    let blocked = access(Fake::new(&[MODEL], serde_json::json!({})), ttl);
    assert!(!blocked.is_allowed(org, MODEL).await);
    assert!(blocked.is_allowed(org, OTHER).await);

    let disabled = access(
        Fake::new(
            &[],
            serde_json::json!({ "long_context_disabled_models": [MODEL] }),
        ),
        ttl,
    );
    assert!(!disabled.is_allowed(org, MODEL).await);
    assert!(disabled.is_allowed(org, OTHER).await);

    let neither = access(
        Fake::new(
            &[],
            serde_json::json!({ "long_context_disabled_models": null }),
        ),
        ttl,
    );
    assert!(neither.is_allowed(org, MODEL).await);

    // An organization without settings (or that does not exist).
    let no_settings = Arc::new(Fake::default());
    assert!(access(no_settings, ttl).is_allowed(org, MODEL).await);

    // A malformed setting turns it off for every model.
    let malformed = access(
        Fake::new(
            &[],
            serde_json::json!({ "long_context_disabled_models": "all" }),
        ),
        ttl,
    );
    assert!(!malformed.is_allowed(org, MODEL).await);
    assert!(!malformed.is_allowed(org, OTHER).await);
}

#[tokio::test]
async fn answers_are_cached_per_organization_for_the_ttl() {
    let repository = Fake::new(&[MODEL], serde_json::json!({}));
    let access = access(repository.clone(), Duration::from_secs(30));
    let (org_a, org_b) = (Uuid::new_v4(), Uuid::new_v4());

    assert!(!access.is_allowed(org_a, MODEL).await);
    assert!(access.is_allowed(org_a, OTHER).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 1);

    // An unblock is not seen until the entry expires.
    repository.blocked.lock().unwrap().clear();
    assert!(!access.is_allowed(org_a, MODEL).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 1);

    // Another organization has its own entry.
    assert!(access.is_allowed(org_b, MODEL).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_failed_read_without_a_previous_value_allows_and_is_cached() {
    let repository = Fake::new(&[MODEL], serde_json::json!({}));
    repository.blocks_fail.store(true, Ordering::SeqCst);
    let access = access(repository.clone(), Duration::from_secs(30));
    let org = Uuid::new_v4();

    assert!(access.is_allowed(org, MODEL).await);
    // The degraded answer is reused for its window: a failing read runs once
    // per organization per window.
    assert!(access.is_allowed(org, MODEL).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_read_keeps_the_last_known_block() {
    let repository = Fake::new(&[MODEL], serde_json::json!({}));
    let access = access(repository.clone(), Duration::from_millis(50));
    let org = Uuid::new_v4();
    assert!(!access.is_allowed(org, MODEL).await);

    expire().await;
    repository.blocks_fail.store(true, Ordering::SeqCst);
    assert!(
        !access.is_allowed(org, MODEL).await,
        "a failed read must not lift a block read before"
    );
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 2);

    // Once reads work again the fresh value wins.
    expire().await;
    repository.blocks_fail.store(false, Ordering::SeqCst);
    repository.blocked.lock().unwrap().clear();
    assert!(access.is_allowed(org, MODEL).await);
}

#[tokio::test]
async fn a_failed_settings_read_keeps_the_last_known_switch() {
    let repository = Fake::new(
        &[],
        serde_json::json!({ "long_context_disabled_models": [MODEL] }),
    );
    let access = access(repository.clone(), Duration::from_millis(50));
    let org = Uuid::new_v4();
    assert!(!access.is_allowed(org, MODEL).await);

    expire().await;
    repository.settings_fail.store(true, Ordering::SeqCst);
    assert!(!access.is_allowed(org, MODEL).await);
}

/// Each input keeps its own last good value: blocks that never load do not
/// stop the organization's switch from being remembered.
#[tokio::test]
async fn the_switch_survives_a_settings_failure_while_blocks_always_fail() {
    let repository = Fake::new(
        &[],
        serde_json::json!({ "long_context_disabled_models": [MODEL] }),
    );
    repository.blocks_fail.store(true, Ordering::SeqCst);
    let access = access(repository.clone(), Duration::from_millis(50));
    let org = Uuid::new_v4();
    assert!(!access.is_allowed(org, MODEL).await);

    expire().await;
    repository.settings_fail.store(true, Ordering::SeqCst);
    assert!(
        !access.is_allowed(org, MODEL).await,
        "the organization's switch must still apply"
    );
    assert!(access.is_allowed(org, OTHER).await);
}

/// The mirror case: settings that never load do not stop a block from being
/// remembered.
#[tokio::test]
async fn the_block_survives_a_block_failure_while_settings_always_fail() {
    let repository = Fake::new(&[MODEL], serde_json::json!({}));
    repository.settings_fail.store(true, Ordering::SeqCst);
    let access = access(repository.clone(), Duration::from_millis(50));
    let org = Uuid::new_v4();
    assert!(!access.is_allowed(org, MODEL).await);

    expire().await;
    repository.blocks_fail.store(true, Ordering::SeqCst);
    assert!(
        !access.is_allowed(org, MODEL).await,
        "the block must still apply"
    );
    assert!(access.is_allowed(org, OTHER).await);
}

#[tokio::test]
async fn a_slow_read_times_out_and_allows() {
    let repository = Arc::new(Fake {
        blocked: Mutex::new(vec![MODEL.to_string()]),
        blocks_delay: Some(Duration::from_secs(5)),
        ..Default::default()
    });
    let access = access(repository, Duration::from_secs(30));
    let started = std::time::Instant::now();
    assert!(access.is_allowed(Uuid::new_v4(), MODEL).await);
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// A read that fails with no last good value falls back to "allowed" for
/// the short cold-fallback TTL only; a fallback that uses a last good value
/// keeps the full TTL.
#[tokio::test]
async fn a_cold_fallback_is_cached_briefly_and_a_warm_one_for_the_full_ttl() {
    let cold_ttl = Duration::from_millis(50);
    let ttl = Duration::from_secs(1);

    // Cold: no previous value.
    let repository = Fake::new(&[MODEL], serde_json::json!({}));
    repository.blocks_fail.store(true, Ordering::SeqCst);
    let cold = LongContextAccess::with_timing(
        repository.clone(),
        ttl,
        cold_ttl,
        Duration::from_millis(200),
    );
    let org = Uuid::new_v4();
    assert!(cold.is_allowed(org, MODEL).await);
    assert!(cold.is_allowed(org, MODEL).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    // The read works again: the block applies after the short window, long
    // before the full TTL.
    repository.blocks_fail.store(false, Ordering::SeqCst);
    assert!(!cold.is_allowed(org, MODEL).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 2);

    // Warm: the fallback uses the last good value and keeps the full TTL.
    let repository = Fake::new(&[MODEL], serde_json::json!({}));
    let warm = LongContextAccess::with_timing(
        repository.clone(),
        ttl,
        cold_ttl,
        Duration::from_millis(200),
    );
    let org = Uuid::new_v4();
    assert!(!warm.is_allowed(org, MODEL).await);
    tokio::time::sleep(ttl + Duration::from_millis(100)).await;
    repository.blocks_fail.store(true, Ordering::SeqCst);
    assert!(!warm.is_allowed(org, MODEL).await);
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 2);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!warm.is_allowed(org, MODEL).await);
    assert_eq!(
        repository.block_reads.load(Ordering::SeqCst),
        2,
        "a fallback with a last good value is reused for the full TTL"
    );
}

/// Concurrent requests of one organization share one load, also when the
/// read fails.
#[tokio::test]
async fn concurrent_misses_share_one_failing_read() {
    let repository = Arc::new(Fake {
        blocks_delay: Some(Duration::from_millis(50)),
        ..Default::default()
    });
    repository.blocks_fail.store(true, Ordering::SeqCst);
    let access = access(repository.clone(), Duration::from_secs(30));
    let org = Uuid::new_v4();
    let answers = futures::future::join_all((0..16).map(|_| access.is_allowed(org, MODEL))).await;
    assert!(answers.into_iter().all(|allowed| allowed));
    assert_eq!(repository.block_reads.load(Ordering::SeqCst), 1);
}
