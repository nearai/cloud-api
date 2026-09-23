use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::*;

struct Repo {
    calls: AtomicUsize,
    delay: Duration,
    fail: bool,
    snapshot: OrganizationAdmissionSnapshot,
}

#[async_trait]
impl AdmissionSnapshotRepository for Repo {
    async fn load_organization(&self, _: Uuid) -> anyhow::Result<OrganizationAdmissionSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            anyhow::bail!("primary unavailable")
        }
        Ok(self.snapshot.clone())
    }
    async fn load_key(&self, org: Uuid, key: Uuid) -> anyhow::Result<KeyAdmissionSnapshot> {
        Ok(KeyAdmissionSnapshot {
            organization_id: org,
            api_key_id: key,
            revision: 1,
            spend_limit: None,
            inference_spent: 0,
        })
    }
}

struct Cache {
    org: Mutex<Option<OrganizationAdmissionSnapshot>>,
    fail_get: bool,
    puts: AtomicUsize,
}

#[async_trait]
impl AdmissionCache for Cache {
    async fn server_time_ms(&self) -> anyhow::Result<i64> {
        Ok(1_000)
    }
    async fn get_organization(
        &self,
        _: Uuid,
    ) -> anyhow::Result<Option<OrganizationAdmissionSnapshot>> {
        if self.fail_get {
            anyhow::bail!("redis unavailable")
        }
        Ok(self.org.lock().await.clone())
    }
    async fn get_key(&self, _: Uuid, _: Uuid) -> anyhow::Result<Option<KeyAdmissionSnapshot>> {
        Ok(None)
    }
    async fn put_organization(
        &self,
        snapshot: &OrganizationAdmissionSnapshot,
        _: i64,
    ) -> anyhow::Result<bool> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        *self.org.lock().await = Some(snapshot.clone());
        Ok(true)
    }
    async fn put_key(&self, _: &KeyAdmissionSnapshot, _: i64) -> anyhow::Result<bool> {
        Ok(true)
    }
}

fn coordinator(repo: Arc<Repo>, cache: Option<Arc<Cache>>) -> AdmissionCoordinator {
    AdmissionCoordinator::new(
        repo,
        cache.map(|cache| cache as Arc<dyn AdmissionCache>),
        config::AdmissionCacheConfig::default(),
        Arc::new(crate::metrics::MockMetricsService),
    )
}

fn snapshot(org: Uuid) -> OrganizationAdmissionSnapshot {
    OrganizationAdmissionSnapshot {
        organization_id: org,
        revision: 1,
        total_spent: None,
        limit: Some(OrganizationLimit {
            spend_limit: 10,
            available: 10,
            unfunded: 0,
        }),
    }
}

#[tokio::test]
async fn cache_hit_avoids_primary_and_miss_fills_cache() {
    let org = Uuid::new_v4();
    let repo = Arc::new(Repo {
        calls: AtomicUsize::new(0),
        delay: Duration::ZERO,
        fail: false,
        snapshot: snapshot(org),
    });
    let cache = Arc::new(Cache {
        org: Mutex::new(Some(snapshot(org))),
        fail_get: false,
        puts: AtomicUsize::new(0),
    });
    let coordinator = coordinator(repo.clone(), Some(cache.clone()));
    assert!(matches!(
        coordinator.check_organization(org).await.unwrap(),
        UsageCheckResult::Allowed { .. }
    ));
    assert_eq!(repo.calls.load(Ordering::SeqCst), 0);
    *cache.org.lock().await = None;
    assert!(coordinator.check_organization(org).await.is_ok());
    assert_eq!(repo.calls.load(Ordering::SeqCst), 1);
    assert_eq!(cache.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn redis_error_falls_back_but_primary_error_fails() {
    let org = Uuid::new_v4();
    let repo = Arc::new(Repo {
        calls: AtomicUsize::new(0),
        delay: Duration::ZERO,
        fail: false,
        snapshot: snapshot(org),
    });
    let cache = Arc::new(Cache {
        org: Mutex::new(None),
        fail_get: true,
        puts: AtomicUsize::new(0),
    });
    assert!(coordinator(repo.clone(), Some(cache))
        .check_organization(org)
        .await
        .is_ok());
    let failing = Arc::new(Repo {
        calls: AtomicUsize::new(0),
        delay: Duration::ZERO,
        fail: true,
        snapshot: snapshot(org),
    });
    let cache = Arc::new(Cache {
        org: Mutex::new(None),
        fail_get: true,
        puts: AtomicUsize::new(0),
    });
    assert!(coordinator(failing, Some(cache))
        .check_organization(org)
        .await
        .is_err());
}

#[tokio::test]
async fn fallback_timeout_does_not_hold_semaphore() {
    let org = Uuid::new_v4();
    let repo = Arc::new(Repo {
        calls: AtomicUsize::new(0),
        delay: Duration::from_millis(100),
        fail: false,
        snapshot: snapshot(org),
    });
    let config = config::AdmissionCacheConfig {
        fallback_deadline_ms: 5,
        fallback_concurrency: 1,
        ..Default::default()
    };
    let coordinator = AdmissionCoordinator::new(
        repo.clone(),
        None,
        config,
        Arc::new(crate::metrics::MockMetricsService),
    );
    assert!(coordinator.check_organization(org).await.is_err());
    assert!(coordinator.check_organization(org).await.is_err());
    assert_eq!(repo.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_shares_read_gate_and_disabled_refresh_never_reads_primary() {
    let org = Uuid::new_v4();
    let repo = Arc::new(Repo {
        calls: AtomicUsize::new(0),
        delay: Duration::ZERO,
        fail: false,
        snapshot: snapshot(org),
    });
    let cache = Arc::new(Cache {
        org: Mutex::new(None),
        fail_get: false,
        puts: AtomicUsize::new(0),
    });
    let config = config::AdmissionCacheConfig {
        fallback_concurrency: 1,
        fallback_deadline_ms: 10,
        ..Default::default()
    };
    let coordinator = AdmissionCoordinator::new(
        repo.clone(),
        Some(cache.clone()),
        config,
        Arc::new(crate::metrics::MockMetricsService),
    );
    let permit = coordinator.fallback_slots.acquire().await.unwrap();
    coordinator.refresh_organization(org).await;
    assert_eq!(
        repo.calls.load(Ordering::SeqCst),
        0,
        "refresh must respect the shared gate"
    );
    assert_eq!(cache.puts.load(Ordering::SeqCst), 0);
    drop(permit);
    coordinator.refresh_organization(org).await;
    assert_eq!(repo.calls.load(Ordering::SeqCst), 1);
    let disabled = AdmissionCoordinator::new(
        repo.clone(),
        None,
        Default::default(),
        Arc::new(crate::metrics::MockMetricsService),
    );
    disabled.refresh_after_usage(org, Uuid::new_v4()).await;
    assert_eq!(repo.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn organization_decision_keeps_missing_credit_and_debt_outcomes() {
    let mut snapshot = snapshot(Uuid::new_v4());
    assert!(matches!(
        evaluate_organization(&snapshot),
        UsageCheckResult::Allowed { remaining: 10 }
    ));
    snapshot.limit.as_mut().unwrap().unfunded = 1;
    assert!(matches!(
        evaluate_organization(&snapshot),
        UsageCheckResult::NoCredits
    ));
    snapshot.total_spent = Some(12);
    assert!(matches!(
        evaluate_organization(&snapshot),
        UsageCheckResult::LimitExceeded {
            spent: 12,
            limit: 10
        }
    ));
    snapshot.limit = None;
    assert!(matches!(
        evaluate_organization(&snapshot),
        UsageCheckResult::NoLimitSet
    ));
    snapshot.total_spent = None;
    assert!(matches!(
        evaluate_organization(&snapshot),
        UsageCheckResult::NoCredits
    ));
}
