//! The record path: the repository wrapper, its queue and the dirty set.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use jiff::Timestamp;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::fake_store::{Call, FakeStore};
use super::ports::StoreErrorKind;
use super::recorder::{SpendTotalRecorder, SpendTotalUsageRepository};
use super::tests::FakeClock;
use super::total_tests::wait_until;
use super::*;
use crate::metrics::capturing::CapturingMetricsService;
use crate::metrics::consts::*;
use crate::usage::budget::BudgetReservation;
use crate::usage::{
    BillingIdentity, InferenceCost, InferenceType, InferenceUsageHistoryQuery,
    InferenceUsageReportRow, OrganizationBalanceInfo, RecordUsageDbRequest, UsageByModelEntry,
    UsageError, UsageLogEntry, UsageRepository,
};

fn request(api_key_id: Uuid, cost: i64) -> RecordUsageDbRequest {
    RecordUsageDbRequest {
        organization_id: Uuid::new_v4(),
        workspace_id: Uuid::new_v4(),
        api_key_id,
        model_id: Uuid::new_v4(),
        model_name: "m".to_string(),
        input_tokens: 1,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        billing_details: None,
        service_tier: None,
        context_band: None,
        input_cost: cost,
        output_cost: 0,
        total_cost: cost,
        inference_type: InferenceType::ChatCompletion,
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: None,
        provider_request_id: None,
        stop_reason: None,
        response_id: None,
        image_count: None,
        provider_attribution: Default::default(),
        dedupe_on_conflict: false,
    }
}

fn entry_for(request: &RecordUsageDbRequest, was_inserted: bool) -> UsageLogEntry {
    UsageLogEntry {
        id: Uuid::new_v4(),
        organization_id: request.organization_id,
        workspace_id: request.workspace_id,
        api_key_id: request.api_key_id,
        model_id: request.model_id,
        model: request.model_name.clone(),
        input_tokens: request.input_tokens,
        output_tokens: request.output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        billing_details: None,
        service_tier: None,
        context_band: None,
        total_tokens: 2,
        input_cost: request.input_cost,
        output_cost: request.output_cost,
        total_cost: request.total_cost,
        inference_type: request.inference_type,
        created_at: Timestamp::now(),
        ttft_ms: None,
        avg_itl_ms: None,
        inference_id: None,
        provider_request_id: None,
        stop_reason: None,
        response_id: None,
        image_count: None,
        was_inserted,
        credit_allocations: None,
        funded_amount: None,
        unfunded_amount: None,
        allocation_policy_version: None,
        provider_attribution: Default::default(),
    }
}

/// Logs each method name; every method answers a distinctive value, and
/// `record_usage` answers per `inserted` / `fail`.
#[derive(Default)]
struct RecordingInner {
    log: Mutex<Vec<String>>,
    inserted: bool,
    fail_record: bool,
}

impl RecordingInner {
    fn log(&self, name: &str) {
        self.log.lock().unwrap().push(name.to_string());
    }
    fn names(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl UsageRepository for RecordingInner {
    async fn mark_budget_unknown(&self, _: Uuid, _: Uuid, grace: i32) -> anyhow::Result<()> {
        self.log(&format!("mark_budget_unknown:{grace}"));
        Ok(())
    }
    async fn release_budget(&self, _: Uuid, _: Uuid) -> anyhow::Result<()> {
        self.log("release_budget");
        Ok(())
    }
    async fn expire_budgets(&self) -> anyhow::Result<u64> {
        self.log("expire_budgets");
        Ok(7)
    }
    async fn admit_budget(&self, reservation: &BudgetReservation) -> Result<(), UsageError> {
        self.log(&format!("admit_budget:{}", reservation.amount));
        Ok(())
    }
    async fn record_usage(&self, request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry> {
        self.log("record_usage");
        if self.fail_record {
            anyhow::bail!("insert failed");
        }
        Ok(entry_for(&request, self.inserted))
    }
    async fn get_balance(&self, _: Uuid) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        self.log("get_balance");
        Ok(None)
    }
    async fn get_usage_history(
        &self,
        _: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        self.log(&format!("get_usage_history:{limit:?}:{offset:?}"));
        Ok((vec![], 11))
    }
    async fn get_usage_history_by_api_key(
        &self,
        _: Uuid,
        credit_type: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        self.log(&format!(
            "get_usage_history_by_api_key:{credit_type:?}:{limit:?}:{offset:?}"
        ));
        Ok((vec![], 12))
    }
    async fn get_api_key_spend(&self, _: Uuid) -> anyhow::Result<i64> {
        self.log("get_api_key_spend");
        Ok(42)
    }
    async fn get_costs_by_inference_ids(
        &self,
        _: Uuid,
        ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        self.log(&format!("get_costs_by_inference_ids:{}", ids.len()));
        Ok(vec![InferenceCost {
            inference_id: ids[0],
            cost_nano_usd: 9,
        }])
    }
    async fn has_client_disconnect_usage(
        &self,
        _: Uuid,
        _: Uuid,
        id: &str,
    ) -> anyhow::Result<bool> {
        self.log(&format!("has_client_disconnect_usage:{id}"));
        Ok(true)
    }
    async fn get_usage_by_model(
        &self,
        _: Uuid,
        _: Timestamp,
        end: Option<Timestamp>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        self.log(&format!("get_usage_by_model:{}", end.is_some()));
        Ok(vec![UsageByModelEntry {
            model: "x".to_string(),
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            total_cost: 4,
            request_count: 5,
        }])
    }
    async fn list_inference_usage_history(
        &self,
        query: InferenceUsageHistoryQuery,
    ) -> anyhow::Result<(Vec<InferenceUsageReportRow>, i64)> {
        self.log(&format!("list_inference_usage_history:{}", query.limit));
        Ok((vec![], 13))
    }
}

struct Rig {
    store: Arc<FakeStore>,
    clock: Arc<FakeClock>,
    metrics: Arc<CapturingMetricsService>,
    shared: Arc<SharedTotal>,
}

impl Rig {
    fn new() -> Self {
        let store = FakeStore::new();
        let clock = FakeClock::new();
        let metrics = Arc::new(CapturingMetricsService::new());
        let shared = SharedTotal::new(store.clone(), clock.clone(), metrics.clone(), TotalMode::On);
        Self {
            store,
            clock,
            metrics,
            shared,
        }
    }

    fn repo(
        &self,
        inner: Arc<RecordingInner>,
        queue: usize,
        dirty: usize,
    ) -> (SpendTotalUsageRepository, Arc<SpendTotalRecorder>) {
        let recorder = SpendTotalRecorder::start_with_capacity(self.shared.clone(), queue, dirty);
        (
            SpendTotalUsageRepository::new(inner, recorder.clone()),
            recorder,
        )
    }

    fn count(&self, name: &str, tag: Option<&str>) -> i64 {
        use crate::metrics::capturing::MetricValue;
        self.metrics
            .get_metrics()
            .iter()
            .filter(|m| m.name == name)
            .filter(|m| tag.is_none_or(|t| m.tags.iter().any(|x| x == t)))
            .map(|m| match m.value {
                MetricValue::Count(n) => n,
                _ => 0,
            })
            .sum()
    }
}

fn inner(inserted: bool) -> Arc<RecordingInner> {
    Arc::new(RecordingInner {
        inserted,
        ..Default::default()
    })
}

// --- when an ADD is queued ---

#[tokio::test]
async fn add_is_enqueued_only_for_inserted_rows_with_positive_cost_and_a_key() {
    let rig = Rig::new();
    let key = Uuid::new_v4();
    // Seed a total so a sent ADD is visible in it.
    rig.store.set_total(key, "1000");
    let cases = [
        (true, 10, key, true),
        (false, 20, key, false), // duplicate row
        (true, 0, key, false),   // free
        (true, -5, key, false),  // negative
        (true, 30, Uuid::nil(), false),
    ];
    for (inserted, cost, id, sent) in cases {
        let (repo, _rec) = rig.repo(inner(inserted), 8, 8);
        let before = rig.store.count(|c| matches!(c, Call::Add(_)));
        let result = repo.record_usage(request(id, cost)).await.unwrap();
        assert_eq!(result.total_cost, cost);
        if sent {
            wait_until("add", || {
                rig.store.count(|c| matches!(c, Call::Add(_))) == before + 1
            })
            .await;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            assert_eq!(
                rig.store.count(|c| matches!(c, Call::Add(_))),
                before,
                "inserted={inserted} cost={cost}"
            );
        }
    }
    assert_eq!(rig.store.total(key).as_deref(), Some("1010"));
}

#[tokio::test]
async fn the_add_carries_the_recorded_cost_of_the_key() {
    let rig = Rig::new();
    let key = Uuid::new_v4();
    let (repo, _rec) = rig.repo(inner(true), 8, 8);
    repo.record_usage(request(key, 77)).await.unwrap();
    wait_until("add", || rig.store.calls_for(key) == vec![Call::Add(77)]).await;
    assert_eq!(
        rig.count(METRIC_API_KEY_SPEND_TOTAL_ADD, Some("result:ok")),
        1
    );
}

// --- failures mark the key dirty ---

#[tokio::test(start_paused = true)]
async fn a_full_queue_marks_the_key_dirty_and_it_is_invalidated_later() {
    let rig = Rig::new();
    let gate = Arc::new(Semaphore::new(0));
    rig.store.gate_adds(gate.clone());
    let (repo, recorder) = rig.repo(inner(true), 1, 8);
    let (k1, k2, k3) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    repo.record_usage(request(k1, 1)).await.unwrap();
    // The task takes k1 and blocks in the store; k2 fills the queue.
    wait_until("k1 in flight", || rig.store.calls_for(k1).len() == 1).await;
    repo.record_usage(request(k2, 1)).await.unwrap();
    repo.record_usage(request(k3, 1)).await.unwrap();
    assert_eq!(rig.count(METRIC_API_KEY_SPEND_TOTAL_QUEUE_FULL, None), 1);
    assert_eq!(recorder.dirty().len(), 1);
    gate.add_permits(10);
    wait_until("k3 invalidated", || {
        rig.store.calls_for(k3).contains(&Call::Invalidate)
    })
    .await;
    assert!(recorder.dirty().is_empty());
    // Only the key that lost its ADD was invalidated.
    assert!(!rig.store.calls_for(k1).contains(&Call::Invalidate));
    assert!(!rig.store.calls_for(k2).contains(&Call::Invalidate));
    assert_eq!(
        rig.count(METRIC_API_KEY_SPEND_TOTAL_DIRTY_INVALIDATED, None),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_add_marks_the_key_dirty_and_it_is_invalidated_once_the_store_answers() {
    let rig = Rig::new();
    let key = Uuid::new_v4();
    rig.store.set_total(key, "500");
    rig.store.fail_with(Some(StoreErrorKind::Conn));
    let (repo, recorder) = rig.repo(inner(true), 8, 8);
    repo.record_usage(request(key, 5)).await.unwrap();
    wait_until("dirty", || recorder.dirty().len() == 1).await;
    assert_eq!(
        rig.count(METRIC_API_KEY_SPEND_TOTAL_ADD, Some("result:error")),
        1
    );
    // Still down and paused: nothing is invalidated, the key stays dirty.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    assert_eq!(recorder.dirty().len(), 1);
    assert_eq!(rig.store.total(key).as_deref(), Some("500"));
    // The store comes back and the pause (fake clock) ends.
    rig.store.fail_with(None);
    rig.clock.advance(6_000);
    wait_until("invalidated", || rig.store.total(key).is_none()).await;
    assert!(recorder.dirty().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_add_skipped_because_the_level_is_paused_marks_the_key_dirty() {
    let rig = Rig::new();
    rig.shared.fail(StoreErrorKind::Conn);
    assert!(rig.shared.paused());
    let key = Uuid::new_v4();
    let (repo, recorder) = rig.repo(inner(true), 8, 8);
    repo.record_usage(request(key, 5)).await.unwrap();
    wait_until("dirty", || recorder.dirty().len() == 1).await;
    assert_eq!(rig.store.calls_for(key), vec![]);
    assert_eq!(
        rig.count(METRIC_API_KEY_SPEND_TOTAL_ADD, Some("result:skipped")),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn dirty_set_overflow_is_counted() {
    let rig = Rig::new();
    rig.shared.fail(StoreErrorKind::Conn);
    let (repo, recorder) = rig.repo(inner(true), 8, 2);
    for _ in 0..3 {
        repo.record_usage(request(Uuid::new_v4(), 5)).await.unwrap();
    }
    wait_until("overflow", || {
        rig.count(METRIC_API_KEY_SPEND_TOTAL_DIRTY_OVERFLOW, None) == 1
    })
    .await;
    assert_eq!(recorder.dirty().len(), 2);
}

// --- the wrapper itself ---

#[tokio::test]
async fn the_wrapper_delegates_every_trait_method() {
    let rig = Rig::new();
    let inner = inner(true);
    let (repo, _rec) = rig.repo(inner.clone(), 8, 8);
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

    repo.mark_budget_unknown(a, b, 33).await.unwrap();
    repo.release_budget(a, b).await.unwrap();
    assert_eq!(repo.expire_budgets().await.unwrap(), 7);
    let reservation = BudgetReservation {
        id: a,
        identity: BillingIdentity {
            organization_id: a,
            workspace_id: a,
            api_key_id: a,
            model_id: a,
        },
        amount: 55,
        inference_type: InferenceType::ChatCompletion,
        estimator_version: 1,
        organization_max_unsettled: 1,
        key_max_unsettled: 1,
        reservation_ttl_seconds: 1,
    };
    repo.admit_budget(&reservation).await.unwrap();
    repo.record_usage(request(Uuid::nil(), 1)).await.unwrap();
    assert!(repo.get_balance(a).await.unwrap().is_none());
    assert_eq!(
        repo.get_usage_history(a, Some(3), Some(4)).await.unwrap().1,
        11
    );
    assert_eq!(
        repo.get_usage_history_by_api_key(a, Some("c"), Some(5), Some(6))
            .await
            .unwrap()
            .1,
        12
    );
    assert_eq!(repo.get_api_key_spend(a).await.unwrap(), 42);
    let costs = repo
        .get_costs_by_inference_ids(a, vec![b, a])
        .await
        .unwrap();
    assert_eq!((costs[0].inference_id, costs[0].cost_nano_usd), (b, 9));
    assert!(repo
        .has_client_disconnect_usage(a, b, "resp_x")
        .await
        .unwrap());
    let by_model = repo
        .get_usage_by_model(a, Timestamp::UNIX_EPOCH, Some(Timestamp::UNIX_EPOCH))
        .await
        .unwrap();
    assert_eq!(by_model[0].request_count, 5);
    let (_, total) = repo
        .list_inference_usage_history(InferenceUsageHistoryQuery {
            organization_id: a,
            start_time: None,
            end_time: None,
            workspace_id: None,
            api_key_id: None,
            credit_type: None,
            limit: 21,
            offset: 0,
        })
        .await
        .unwrap();
    assert_eq!(total, 13);

    // One entry per method, in call order, with the arguments forwarded.
    assert_eq!(
        inner.names(),
        vec![
            "mark_budget_unknown:33",
            "release_budget",
            "expire_budgets",
            "admit_budget:55",
            "record_usage",
            "get_balance",
            "get_usage_history:Some(3):Some(4)",
            "get_usage_history_by_api_key:Some(\"c\"):Some(5):Some(6)",
            "get_api_key_spend",
            "get_costs_by_inference_ids:2",
            "has_client_disconnect_usage:resp_x",
            "get_usage_by_model:true",
            "list_inference_usage_history:21",
        ]
    );
}

#[tokio::test]
async fn the_wrapper_returns_the_inner_result_unchanged_even_when_the_store_fails() {
    let rig = Rig::new();
    rig.store.fail_with(Some(StoreErrorKind::NoPerm));
    let inner = inner(true);
    let (repo, _rec) = rig.repo(inner.clone(), 8, 8);
    let key = Uuid::new_v4();
    let req = request(key, 9);
    let (org, ws) = (req.organization_id, req.workspace_id);
    let entry = repo.record_usage(req).await.unwrap();
    assert_eq!(
        (
            entry.organization_id,
            entry.workspace_id,
            entry.api_key_id,
            entry.total_cost
        ),
        (org, ws, key, 9)
    );
    assert!(entry.was_inserted);
    assert_eq!(inner.names(), vec!["record_usage"]);
}

#[tokio::test]
async fn an_inner_error_is_returned_and_nothing_is_added() {
    let rig = Rig::new();
    let inner = Arc::new(RecordingInner {
        inserted: true,
        fail_record: true,
        ..Default::default()
    });
    let (repo, _rec) = rig.repo(inner, 8, 8);
    let key = Uuid::new_v4();
    assert!(repo.record_usage(request(key, 9)).await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(rig.store.calls().is_empty());
}
