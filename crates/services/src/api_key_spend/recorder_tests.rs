//! The record path of the local budget: the repository wrapper.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use uuid::Uuid;

use super::budget::BudgetSettings;
use super::recorder::BudgetUsageRepository;
use super::tests::FakeClock;
use super::*;
use crate::metrics::capturing::CapturingMetricsService;
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
    costs: Mutex<Vec<i64>>,
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
        self.costs.lock().unwrap().push(request.total_cost);
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

struct FixedSource;

#[async_trait]
impl ports::ApiKeySpendSource for FixedSource {
    async fn api_key_spend(&self, _id: Uuid) -> anyhow::Result<i64> {
        Ok(0)
    }
}

fn budget_cache() -> ApiKeySpendCache {
    ApiKeySpendCache::new_with_budget(
        10,
        Duration::from_secs(30),
        Arc::new(FixedSource),
        Arc::new(CapturingMetricsService::new()),
        FakeClock::new(),
        Some(BudgetSettings {
            share_percent: 50,
            max_age_secs: 60,
        }),
    )
}

fn inner(inserted: bool) -> Arc<RecordingInner> {
    Arc::new(RecordingInner {
        inserted,
        ..Default::default()
    })
}

async fn used(cache: &ApiKeySpendCache, key: Uuid) -> i64 {
    cache.budget_snapshot(key).await.unwrap().2
}

// --- when a cost is added ---

#[tokio::test]
async fn a_cost_is_added_only_for_inserted_rows_with_a_positive_cost_and_a_key() {
    let cache = budget_cache();
    let key = Uuid::new_v4();
    // Gives the key budget state (the limit is far above the costs below).
    cache.check(key, i64::MAX).await.unwrap();
    let cases = [
        (true, 10, key, 10),
        (false, 20, key, 0), // the row existed already
        (true, 0, key, 0),   // free
        (true, -5, key, 0),  // negative
    ];
    for (inserted, cost, id, expected_add) in cases {
        let repo = BudgetUsageRepository::new(inner(inserted), cache.clone());
        let before = used(&cache, key).await;
        let result = repo.record_usage(request(id, cost)).await.unwrap();
        assert_eq!(result.total_cost, cost);
        assert_eq!(
            used(&cache, key).await - before,
            expected_add,
            "{inserted} {cost}"
        );
    }
    // A nil key id is ignored.
    let repo = BudgetUsageRepository::new(inner(true), cache.clone());
    repo.record_usage(request(Uuid::nil(), 30)).await.unwrap();
    assert_eq!(used(&cache, key).await, 10);
}

#[tokio::test]
async fn a_failed_insert_adds_nothing_and_the_error_is_returned() {
    let cache = budget_cache();
    let key = Uuid::new_v4();
    cache.check(key, i64::MAX).await.unwrap();
    let failing = Arc::new(RecordingInner {
        inserted: true,
        fail_record: true,
        ..Default::default()
    });
    let repo = BudgetUsageRepository::new(failing, cache.clone());
    assert!(repo.record_usage(request(key, 10)).await.is_err());
    assert_eq!(used(&cache, key).await, 0);
}

#[tokio::test]
async fn costs_of_keys_without_state_are_dropped() {
    let cache = budget_cache();
    let repo = BudgetUsageRepository::new(inner(true), cache.clone());
    let key = Uuid::new_v4();
    repo.record_usage(request(key, 10)).await.unwrap();
    cache.check(key, i64::MAX).await.unwrap();
    assert_eq!(used(&cache, key).await, 0);
}

// --- the wrapper itself ---

#[tokio::test]
async fn the_wrapper_delegates_every_trait_method() {
    let inner = inner(true);
    let repo = BudgetUsageRepository::new(inner.clone(), budget_cache());
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

// --- through the real usage service ---

struct PricedModels;

#[async_trait]
impl crate::usage::ModelRepository for PricedModels {
    async fn get_model_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<crate::usage::ModelPricing>> {
        Ok(Some(priced(name)))
    }
    async fn get_model_by_id(&self, _: Uuid) -> anyhow::Result<Option<crate::usage::ModelPricing>> {
        Ok(Some(priced("m")))
    }
}

fn priced(name: &str) -> crate::usage::ModelPricing {
    crate::usage::ModelPricing {
        id: Uuid::new_v4(),
        model_name: name.to_string(),
        input_cost_per_token: 3,
        output_cost_per_token: 7,
        cost_per_image: 11,
        cache_read_cost_per_token: Some(1),
        text_pricing: None,
    }
}

struct NoLimits;

#[async_trait]
impl crate::usage::OrganizationLimitsRepository for NoLimits {
    async fn get_current_limits(
        &self,
        _: Uuid,
    ) -> anyhow::Result<Option<crate::usage::OrganizationLimit>> {
        unimplemented!()
    }
    async fn get_current_limit_breakdown(
        &self,
        _: Uuid,
    ) -> anyhow::Result<Vec<crate::usage::OrganizationCreditLimit>> {
        unimplemented!()
    }
}

/// The usage service, built as the application builds it (the repository it
/// records through is the budget wrapper), feeds the budget's `used` with the
/// cost it stored. The repository underneath is a fake, so this proves the
/// wrapper sits on the service's record path and counts the stored cost; it
/// does not exercise the SQL repository or the application's wiring code.
#[tokio::test]
async fn a_cost_recorded_through_the_usage_service_reaches_the_budget() {
    use crate::usage::UsageServiceTrait;

    let cache = budget_cache();
    let fake = inner(true);
    let repo = Arc::new(BudgetUsageRepository::new(fake.clone(), cache.clone()));
    let service = crate::usage::UsageServiceImpl::new(
        repo,
        Arc::new(PricedModels),
        Arc::new(NoLimits),
        Arc::new(crate::workspace::MockWorkspaceServiceTrait::new()),
        Arc::new(CapturingMetricsService::new()),
    );
    let key = Uuid::new_v4();
    cache.check(key, i64::MAX).await.unwrap();
    service
        .record_usage(crate::usage::RecordUsageServiceRequest {
            budget_id: None,
            organization_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            api_key_id: key,
            model_id: Uuid::new_v4(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write: None,
            profiled_cache_write_tokens: 0,
            requested_service_tier: None,
            provider_service_tier: None,
            inference_type: InferenceType::ChatCompletion,
            ttft_ms: None,
            avg_itl_ms: None,
            inference_id: None,
            provider_request_id: None,
            stop_reason: None,
            response_id: None,
            image_count: None,
            provider_attribution: Default::default(),
            discount: None,
            dedupe_on_conflict: false,
        })
        .await
        .expect("record_usage");
    let stored = fake.costs.lock().unwrap().clone();
    assert_eq!(stored.len(), 1);
    assert!(stored[0] > 0);
    assert_eq!(used(&cache, key).await, stored[0]);
}
