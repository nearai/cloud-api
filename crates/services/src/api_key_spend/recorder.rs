//! The record path of the local budget: after a usage row is stored, its cost
//! is added to the key's `used` counter.
//!
//! [`BudgetUsageRepository`] wraps the usage repository. A cost is added only
//! when this call inserted its row; a call that fails, or whose row existed
//! already, adds nothing (the next refresh reads the database and sees what
//! was stored).

use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use uuid::Uuid;

use super::ApiKeySpendCache;
use crate::usage::budget::BudgetReservation;
use crate::usage::{
    InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportRow, OrganizationBalanceInfo,
    RecordUsageDbRequest, UsageByModelEntry, UsageError, UsageLogEntry, UsageRepository,
};

/// A usage repository that counts each newly recorded cost in the local
/// spend budget. It adds in memory after the write
/// returned, never changes what the inner call returns, and counts only keys
/// that have budget state. Every method delegates to the inner repository,
/// including the ones the trait gives default bodies (those defaults return
/// errors).
pub struct BudgetUsageRepository {
    inner: Arc<dyn UsageRepository>,
    cache: ApiKeySpendCache,
}

impl BudgetUsageRepository {
    pub fn new(inner: Arc<dyn UsageRepository>, cache: ApiKeySpendCache) -> Self {
        Self { inner, cache }
    }
}

#[async_trait]
impl UsageRepository for BudgetUsageRepository {
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
        // A repeated insert (the row existed already) adds nothing: its cost
        // was counted when the row was first written.
        if entry.was_inserted && entry.total_cost > 0 && !entry.api_key_id.is_nil() {
            self.cache
                .record_cost(entry.api_key_id, entry.total_cost)
                .await;
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
