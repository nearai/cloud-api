use super::*;
use crate::metrics::capturing::{CapturingMetricsService, MetricValue};
use crate::test_utils::{MockAttestationService, MockUsageService};
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex as StdMutex;
use std::time::Instant;

const LIMIT: u32 = 3;
const MODEL_NAME: &str = "test/model";

/// Stands in for the lease table shared by every replica. Admission is
/// serialised the way the advisory lock serialises it in Postgres.
#[derive(Default)]
struct SharedLeaseStore {
    rows: StdMutex<StdHashMap<Uuid, (Uuid, Uuid, Instant)>>,
    limit: StdMutex<Option<u32>>,
    unavailable: std::sync::atomic::AtomicBool,
}

impl SharedLeaseStore {
    fn with_limit(limit: u32) -> Self {
        Self {
            limit: StdMutex::new(Some(limit)),
            ..Default::default()
        }
    }

    fn set_unavailable(&self, unavailable: bool) {
        self.unavailable
            .store(unavailable, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_unavailable(&self) -> bool {
        self.unavailable.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn live_count(&self, organization_id: Uuid, model_id: Uuid) -> usize {
        let rows = self.rows.lock().unwrap();
        rows.values()
            .filter(|(org, model, expires)| {
                *org == organization_id && *model == model_id && *expires > Instant::now()
            })
            .count()
    }
}

#[async_trait::async_trait]
impl ports::ConcurrencyLeaseRepository for SharedLeaseStore {
    async fn try_acquire(
        &self,
        lease_id: Uuid,
        organization_id: Uuid,
        model_id: Uuid,
        _instance_id: &str,
        default_limit: u32,
        ttl: Duration,
    ) -> Result<ports::LeaseOutcome, anyhow::Error> {
        if self.is_unavailable() {
            anyhow::bail!("lease store unavailable");
        }

        let limit = self.limit.lock().unwrap().unwrap_or(default_limit);
        let mut rows = self.rows.lock().unwrap();
        let in_flight = rows
            .values()
            .filter(|(org, model, expires)| {
                *org == organization_id && *model == model_id && *expires > Instant::now()
            })
            .count();

        if in_flight >= limit as usize {
            return Ok(ports::LeaseOutcome::AtLimit {
                limit,
                in_flight: in_flight as i64,
            });
        }

        rows.insert(lease_id, (organization_id, model_id, Instant::now() + ttl));
        Ok(ports::LeaseOutcome::Admitted { limit })
    }

    async fn release(&self, lease_ids: &[Uuid]) -> Result<(), anyhow::Error> {
        let mut rows = self.rows.lock().unwrap();
        for id in lease_ids {
            rows.remove(id);
        }
        Ok(())
    }

    async fn renew(&self, lease_ids: &[Uuid], ttl: Duration) -> Result<Vec<Uuid>, anyhow::Error> {
        if self.is_unavailable() {
            anyhow::bail!("lease store unavailable");
        }
        let mut rows = self.rows.lock().unwrap();
        let mut renewed = Vec::new();
        for id in lease_ids {
            if let Some(row) = rows.get_mut(id) {
                row.2 = Instant::now() + ttl;
                renewed.push(*id);
            }
        }
        Ok(renewed)
    }

    async fn persist(
        &self,
        leases: &[ports::HeldLease],
        _instance_id: &str,
        ttl: Duration,
    ) -> Result<(), anyhow::Error> {
        if self.is_unavailable() {
            anyhow::bail!("lease store unavailable");
        }
        let mut rows = self.rows.lock().unwrap();
        for lease in leases {
            rows.insert(
                lease.id,
                (lease.organization_id, lease.model_id, Instant::now() + ttl),
            );
        }
        Ok(())
    }

    async fn sweep_expired(&self) -> Result<u64, anyhow::Error> {
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|_, (_, _, expires)| *expires > Instant::now());
        Ok((before - rows.len()) as u64)
    }
}

/// Admission never consults the model catalogue, so this only has to exist.
struct EmptyModelsRepository;

#[async_trait::async_trait]
impl crate::models::ModelsRepository for EmptyModelsRepository {
    async fn get_all_active_models(
        &self,
    ) -> Result<Vec<crate::models::ModelWithPricing>, anyhow::Error> {
        Ok(Vec::new())
    }

    async fn get_model_by_name(
        &self,
        _name: &str,
    ) -> Result<Option<crate::models::ModelWithPricing>, anyhow::Error> {
        Ok(None)
    }

    async fn resolve_and_get_model(
        &self,
        _identifier: &str,
    ) -> Result<Option<crate::models::ModelWithPricing>, anyhow::Error> {
        Ok(None)
    }

    async fn get_configured_model_names(&self) -> Result<Vec<String>, anyhow::Error> {
        Ok(Vec::new())
    }
}

struct StaticLimitRepository(Option<u32>);

#[async_trait::async_trait]
impl ports::OrganizationConcurrentLimitRepository for StaticLimitRepository {
    async fn get_concurrent_limit(&self, _org_id: Uuid) -> Result<Option<u32>, anyhow::Error> {
        Ok(self.0)
    }
}

struct FailingLimitRepository;

#[async_trait::async_trait]
impl ports::OrganizationConcurrentLimitRepository for FailingLimitRepository {
    async fn get_concurrent_limit(&self, _org_id: Uuid) -> Result<Option<u32>, anyhow::Error> {
        anyhow::bail!("limit lookup unavailable")
    }
}

fn replica(
    store: Arc<SharedLeaseStore>,
    metrics: Arc<CapturingMetricsService>,
    limits: Arc<dyn ports::OrganizationConcurrentLimitRepository>,
) -> CompletionServiceImpl {
    replica_with(store, metrics, limits, true)
}

fn replica_with(
    store: Arc<SharedLeaseStore>,
    metrics: Arc<CapturingMetricsService>,
    limits: Arc<dyn ports::OrganizationConcurrentLimitRepository>,
    enforcing: bool,
) -> CompletionServiceImpl {
    let pool = Arc::new(InferenceProviderPool::new(
        None,
        config::ExternalProvidersConfig::default(),
    ));
    CompletionServiceImpl::new(
        pool,
        Arc::new(MockAttestationService),
        Arc::new(MockUsageService),
        metrics,
        Arc::new(EmptyModelsRepository),
        limits,
    )
    .with_fleet_concurrency(
        store,
        "test-instance".to_string(),
        Duration::from_secs(60),
        enforcing,
    )
}

fn counts(metrics: &CapturingMetricsService, name: &str) -> usize {
    metrics
        .get_metrics()
        .into_iter()
        .filter(|metric| metric.name == name && matches!(metric.value, MetricValue::Count(_)))
        .count()
}

/// The invariant the issue turns on: several replicas sharing one lease store
/// admit no more than the organization's limit between them.
#[tokio::test]
async fn replicas_share_one_limit() {
    const REPLICAS: usize = 4;
    const ATTEMPTS: usize = 20;

    let store = Arc::new(SharedLeaseStore::with_limit(LIMIT));
    let metrics = Arc::new(CapturingMetricsService::new());
    let organization_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();

    let replicas: Vec<Arc<CompletionServiceImpl>> = (0..REPLICAS)
        .map(|_| {
            Arc::new(replica(
                store.clone(),
                metrics.clone(),
                Arc::new(StaticLimitRepository(Some(LIMIT))),
            ))
        })
        .collect();

    let mut slots = Vec::new();
    for attempt in 0..ATTEMPTS {
        let service = &replicas[attempt % REPLICAS];
        if let Ok(slot) = service
            .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
        {
            slots.push(slot);
        }
    }

    assert_eq!(
        slots.len(),
        LIMIT as usize,
        "{REPLICAS} replicas admitted {} against a shared limit of {LIMIT}",
        slots.len()
    );
    assert_eq!(store.live_count(organization_id, model_id), LIMIT as usize);
    assert_eq!(
        counts(&metrics, METRIC_CONCURRENCY_ADMITTED),
        LIMIT as usize
    );
    assert_eq!(
        counts(&metrics, METRIC_CONCURRENCY_REJECTED),
        ATTEMPTS - LIMIT as usize
    );
}

/// Releasing on one replica must free capacity for a different replica.
#[tokio::test]
async fn capacity_freed_on_one_replica_is_visible_to_another() {
    let store = Arc::new(SharedLeaseStore::with_limit(1));
    let metrics = Arc::new(CapturingMetricsService::new());
    let organization_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();

    let first = replica(
        store.clone(),
        metrics.clone(),
        Arc::new(StaticLimitRepository(Some(1))),
    );
    let second = replica(
        store.clone(),
        metrics.clone(),
        Arc::new(StaticLimitRepository(Some(1))),
    );

    // The guard is what releases; a bare slot has no Drop of its own.
    let guard = ConcurrentSlotGuard::new(
        first
            .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
            .expect("first replica admitted"),
    );
    assert!(second
        .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
        .await
        .is_err());

    drop(guard);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(
        second
            .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
            .is_ok(),
        "a slot released on one replica must become usable on another"
    );
}

/// With the store unreachable a replica counts only its own leases, which is
/// the behaviour that shipped before fleet limits. It must never admit a fresh
/// limit on top of leases already live.
#[tokio::test]
async fn degraded_admission_does_not_stack_on_live_leases() {
    let store = Arc::new(SharedLeaseStore::with_limit(LIMIT));
    let metrics = Arc::new(CapturingMetricsService::new());
    let organization_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();

    let service = replica(
        store.clone(),
        metrics.clone(),
        Arc::new(StaticLimitRepository(Some(LIMIT))),
    );

    let mut slots = Vec::new();
    for _ in 0..LIMIT {
        slots.push(
            service
                .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
                .await
                .expect("admitted while healthy"),
        );
    }

    store.set_unavailable(true);

    assert!(
        service
            .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
            .is_err(),
        "the degraded path must count leases this replica already holds"
    );
    assert_eq!(counts(&metrics, METRIC_CONCURRENCY_DEGRADED), 1);
}

/// A failed limit lookup must not widen the cap. The organization's real limit
/// was read once, so it is used rather than the global default.
#[tokio::test]
async fn a_failed_limit_lookup_keeps_the_known_limit() {
    // The organization's real limit is 1, well under the global default of 64.
    let store = Arc::new(SharedLeaseStore::with_limit(1));
    let metrics = Arc::new(CapturingMetricsService::new());
    let organization_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();

    let service = replica(
        store.clone(),
        metrics.clone(),
        Arc::new(StaticLimitRepository(Some(1))),
    );

    let _slot = service
        .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
        .await
        .expect("admitted while healthy");

    store.set_unavailable(true);

    assert!(
        service
            .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
            .is_err(),
        "an outage must not raise the cap to the global default"
    );
}

#[tokio::test]
async fn an_unknown_limit_falls_back_to_the_default() {
    let store = Arc::new(SharedLeaseStore::default());
    let metrics = Arc::new(CapturingMetricsService::new());
    let service = replica(
        store.clone(),
        metrics.clone(),
        Arc::new(FailingLimitRepository),
    );

    store.set_unavailable(true);
    let slot = service
        .try_acquire_concurrent_slot(Uuid::new_v4(), Uuid::new_v4(), MODEL_NAME)
        .await;

    assert!(
        slot.is_ok(),
        "with no limit ever read the default applies rather than rejecting"
    );
}

/// Shadow mode is only useful if it measures without rejecting: the fleet
/// count goes over the limit and the request still proceeds.
#[tokio::test]
async fn shadowing_records_the_verdict_without_rejecting() {
    const REPLICAS: usize = 4;
    const ATTEMPTS: usize = 20;

    let store = Arc::new(SharedLeaseStore::with_limit(LIMIT));
    let metrics = Arc::new(CapturingMetricsService::new());
    let organization_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();

    let replicas: Vec<Arc<CompletionServiceImpl>> = (0..REPLICAS)
        .map(|_| {
            Arc::new(replica_with(
                store.clone(),
                metrics.clone(),
                Arc::new(StaticLimitRepository(Some(LIMIT))),
                false,
            ))
        })
        .collect();

    let mut slots = Vec::new();
    for attempt in 0..ATTEMPTS {
        let service = &replicas[attempt % REPLICAS];
        if let Ok(slot) = service
            .try_acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
        {
            slots.push(slot);
        }
    }

    assert!(
        slots.len() > LIMIT as usize,
        "shadowing must not enforce the fleet limit, admitted {}",
        slots.len()
    );
    assert!(
        counts(&metrics, METRIC_CONCURRENCY_WOULD_REJECT) > 0,
        "the over-limit verdict is the whole signal shadow mode exists to give"
    );
    assert_eq!(
        counts(&metrics, METRIC_CONCURRENCY_REJECTED),
        0,
        "nothing is rejected while shadowing"
    );
}

/// The guard handed to direct transport routes must free fleet capacity when
/// dropped, or those routes hold leases until the TTL expires.
#[tokio::test]
async fn dropping_a_transport_guard_frees_fleet_capacity() {
    use ports::CompletionServiceTrait;

    let store = Arc::new(SharedLeaseStore::with_limit(1));
    let metrics = Arc::new(CapturingMetricsService::new());
    let organization_id = Uuid::new_v4();
    let model_id = Uuid::new_v4();
    let service = replica(
        store.clone(),
        metrics.clone(),
        Arc::new(StaticLimitRepository(Some(1))),
    );

    let guard = service
        .acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
        .await
        .expect("the first request takes the only slot");
    assert_eq!(store.live_count(organization_id, model_id), 1);

    assert!(
        service
            .acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
            .await
            .is_err(),
        "the limit is one, so a second transport request is refused"
    );

    drop(guard);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        store.live_count(organization_id, model_id),
        0,
        "the released lease must leave the store, not linger until it expires"
    );
    service
        .acquire_concurrent_slot(organization_id, model_id, MODEL_NAME)
        .await
        .expect("the freed slot admits the next transport request");
}
