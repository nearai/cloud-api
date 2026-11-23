//! The record path: after a usage row commits, its cost is added to the
//! shared total.
//!
//! [`SpendTotalUsageRepository`] wraps the usage repository and never changes
//! what the inner call returns or when. The ADD is queued (bounded) and sent
//! by one background task. When it cannot be sent (queue full, the store
//! failed, the shared level is paused) the key goes into a bounded dirty set,
//! and the same task invalidates dirty keys once the store answers again, so
//! a total that missed a cost is dropped and re-seeded from the database.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::total::{SharedTotal, BACKGROUND_TIMEOUT};
use crate::metrics::consts::{
    METRIC_API_KEY_SPEND_TOTAL_ADD, METRIC_API_KEY_SPEND_TOTAL_DIRTY_INVALIDATED,
    METRIC_API_KEY_SPEND_TOTAL_DIRTY_OVERFLOW, METRIC_API_KEY_SPEND_TOTAL_QUEUE_FULL,
};
use crate::usage::budget::BudgetReservation;
use crate::usage::{
    InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportRow, OrganizationBalanceInfo,
    RecordUsageDbRequest, UsageByModelEntry, UsageError, UsageLogEntry, UsageRepository,
};

/// Capacity of the ADD queue.
pub const QUEUE_CAPACITY: usize = 1024;
/// Most keys in the dirty set; further keys are counted and dropped.
pub const DIRTY_CAPACITY: usize = 10_000;
/// How often the task looks at the dirty set.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Keys invalidated per flush.
const FLUSH_BATCH: usize = 64;

/// Keys whose total may be missing a cost.
pub struct DirtySet {
    keys: Mutex<HashSet<Uuid>>,
    capacity: usize,
}

impl DirtySet {
    fn new(capacity: usize) -> Self {
        Self {
            keys: Mutex::new(HashSet::new()),
            capacity,
        }
    }

    /// Returns `false` when the set is full and the key was dropped.
    fn mark(&self, api_key_id: Uuid) -> bool {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        if keys.len() >= self.capacity && !keys.contains(&api_key_id) {
            return false;
        }
        keys.insert(api_key_id);
        true
    }

    fn take_batch(&self, max: usize) -> Vec<Uuid> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let batch: Vec<Uuid> = keys.iter().copied().take(max).collect();
        for id in &batch {
            keys.remove(id);
        }
        batch
    }

    pub fn len(&self) -> usize {
        self.keys.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Queues ADDs for the background task.
pub struct SpendTotalRecorder {
    tx: mpsc::Sender<(Uuid, i64)>,
    dirty: Arc<DirtySet>,
    shared: Arc<SharedTotal>,
}

impl SpendTotalRecorder {
    /// Starts the background task. Needs a running tokio runtime; the task
    /// ends when the recorder is dropped.
    pub fn start(shared: Arc<SharedTotal>) -> Arc<Self> {
        Self::start_with_capacity(shared, QUEUE_CAPACITY, DIRTY_CAPACITY)
    }

    pub(super) fn start_with_capacity(
        shared: Arc<SharedTotal>,
        queue: usize,
        dirty_capacity: usize,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(queue);
        let dirty = Arc::new(DirtySet::new(dirty_capacity));
        tokio::spawn(drain(rx, dirty.clone(), shared.clone()));
        Arc::new(Self { tx, dirty, shared })
    }

    /// Queues the ADD of a committed cost. Never blocks and never fails.
    pub fn record(&self, api_key_id: Uuid, cost: i64) {
        if self.tx.try_send((api_key_id, cost)).is_err() {
            self.shared
                .count(METRIC_API_KEY_SPEND_TOTAL_QUEUE_FULL, &[]);
            mark_dirty(&self.dirty, &self.shared, api_key_id);
        }
    }

    pub fn dirty(&self) -> &DirtySet {
        &self.dirty
    }
}

fn mark_dirty(dirty: &DirtySet, shared: &SharedTotal, api_key_id: Uuid) {
    if !dirty.mark(api_key_id) {
        shared.count(METRIC_API_KEY_SPEND_TOTAL_DIRTY_OVERFLOW, &[]);
    }
}

async fn drain(
    mut rx: mpsc::Receiver<(Uuid, i64)>,
    dirty: Arc<DirtySet>,
    shared: Arc<SharedTotal>,
) {
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            message = rx.recv() => match message {
                Some((api_key_id, cost)) => add(&dirty, &shared, api_key_id, cost).await,
                None => return,
            },
            _ = tick.tick() => flush_dirty(&dirty, &shared).await,
        }
    }
}

async fn add(dirty: &DirtySet, shared: &SharedTotal, api_key_id: Uuid, cost: i64) {
    if shared.paused() {
        shared.count_result(METRIC_API_KEY_SPEND_TOTAL_ADD, "skipped");
        mark_dirty(dirty, shared, api_key_id);
        return;
    }
    match shared
        .call(BACKGROUND_TIMEOUT, shared.store().add(api_key_id, cost))
        .await
    {
        Some(_) => shared.count_result(METRIC_API_KEY_SPEND_TOTAL_ADD, "ok"),
        None => {
            shared.count_result(METRIC_API_KEY_SPEND_TOTAL_ADD, "error");
            mark_dirty(dirty, shared, api_key_id);
        }
    }
}

async fn flush_dirty(dirty: &DirtySet, shared: &SharedTotal) {
    if dirty.is_empty() || shared.paused() {
        return;
    }
    let batch = dirty.take_batch(FLUSH_BATCH);
    for (i, api_key_id) in batch.iter().enumerate() {
        let done = shared
            .call(BACKGROUND_TIMEOUT, shared.store().invalidate(*api_key_id))
            .await;
        if done.is_none() {
            // The store is down again: keep this key and the rest for later.
            for id in &batch[i..] {
                mark_dirty(dirty, shared, *id);
            }
            return;
        }
        shared.count(METRIC_API_KEY_SPEND_TOTAL_DIRTY_INVALIDATED, &[]);
    }
}

/// A usage repository that adds each newly recorded cost to the shared total.
/// Every method delegates to the inner repository, including the ones the
/// trait gives default bodies (those defaults return errors).
pub struct SpendTotalUsageRepository {
    inner: Arc<dyn UsageRepository>,
    recorder: Arc<SpendTotalRecorder>,
}

impl SpendTotalUsageRepository {
    pub fn new(inner: Arc<dyn UsageRepository>, recorder: Arc<SpendTotalRecorder>) -> Self {
        Self { inner, recorder }
    }
}

#[async_trait]
impl UsageRepository for SpendTotalUsageRepository {
    async fn mark_budget_unknown(
        &self,
        organization_id: Uuid,
        id: Uuid,
        grace_seconds: i32,
    ) -> anyhow::Result<()> {
        self.inner
            .mark_budget_unknown(organization_id, id, grace_seconds)
            .await
    }

    async fn release_budget(&self, organization_id: Uuid, id: Uuid) -> anyhow::Result<()> {
        self.inner.release_budget(organization_id, id).await
    }

    async fn expire_budgets(&self) -> anyhow::Result<u64> {
        self.inner.expire_budgets().await
    }

    async fn admit_budget(&self, reservation: &BudgetReservation) -> Result<(), UsageError> {
        self.inner.admit_budget(reservation).await
    }

    async fn record_usage(&self, request: RecordUsageDbRequest) -> anyhow::Result<UsageLogEntry> {
        let entry = self.inner.record_usage(request).await?;
        if entry.was_inserted && entry.total_cost > 0 && !entry.api_key_id.is_nil() {
            self.recorder.record(entry.api_key_id, entry.total_cost);
        }
        Ok(entry)
    }

    async fn get_balance(
        &self,
        organization_id: Uuid,
    ) -> anyhow::Result<Option<OrganizationBalanceInfo>> {
        self.inner.get_balance(organization_id).await
    }

    async fn get_usage_history(
        &self,
        organization_id: Uuid,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        self.inner
            .get_usage_history(organization_id, limit, offset)
            .await
    }

    async fn get_usage_history_by_api_key(
        &self,
        api_key_id: Uuid,
        credit_type: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<(Vec<UsageLogEntry>, i64)> {
        self.inner
            .get_usage_history_by_api_key(api_key_id, credit_type, limit, offset)
            .await
    }

    async fn get_api_key_spend(&self, api_key_id: Uuid) -> anyhow::Result<i64> {
        self.inner.get_api_key_spend(api_key_id).await
    }

    async fn get_costs_by_inference_ids(
        &self,
        organization_id: Uuid,
        inference_ids: Vec<Uuid>,
    ) -> anyhow::Result<Vec<InferenceCost>> {
        self.inner
            .get_costs_by_inference_ids(organization_id, inference_ids)
            .await
    }

    async fn has_client_disconnect_usage(
        &self,
        organization_id: Uuid,
        workspace_id: Uuid,
        id: &str,
    ) -> anyhow::Result<bool> {
        self.inner
            .has_client_disconnect_usage(organization_id, workspace_id, id)
            .await
    }

    async fn get_usage_by_model(
        &self,
        organization_id: Uuid,
        start_date: Timestamp,
        end_date: Option<Timestamp>,
    ) -> anyhow::Result<Vec<UsageByModelEntry>> {
        self.inner
            .get_usage_by_model(organization_id, start_date, end_date)
            .await
    }

    async fn list_inference_usage_history(
        &self,
        query: InferenceUsageHistoryQuery,
    ) -> anyhow::Result<(Vec<InferenceUsageReportRow>, i64)> {
        self.inner.list_inference_usage_history(query).await
    }
}
