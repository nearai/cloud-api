use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use services::usage::{UsageCheckResult, UsageServiceTrait};
use std::{future::Future, pin::Pin, sync::Arc};
use tracing::{debug, warn};

use super::auth::AuthenticatedApiKey;
use crate::models::{AnthropicErrorResponse, ErrorResponse};
use crate::routes::common::format_amount;

pub trait StakingFarmPreflightSync: Send + Sync {
    fn sync_organization_if_stale(
        &self,
        organization_id: uuid::Uuid,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = anyhow::Result<
                        Option<services::staking_farm::OrganizationStakingFarmSource>,
                    >,
                > + Send
                + '_,
        >,
    >;
}

impl StakingFarmPreflightSync for services::staking_farm::StakingFarmService {
    fn sync_organization_if_stale(
        &self,
        organization_id: uuid::Uuid,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = anyhow::Result<
                        Option<services::staking_farm::OrganizationStakingFarmSource>,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self.sync_organization_if_stale(organization_id))
    }
}

/// State for usage middleware
#[derive(Clone)]
pub struct UsageState {
    pub usage_service: Arc<dyn UsageServiceTrait + Send + Sync>,
    pub staking_farm_service: Arc<services::staking_farm::StakingFarmService>,
    pub usage_repository: Arc<database::repositories::OrganizationUsageRepository>,
    pub api_key_repository: Arc<database::repositories::ApiKeyRepository>,
}

/// Result of reading a key's accumulated spend, for keys that carry a spend
/// limit. `Err` means the read failed.
type ApiKeySpendRead = Result<i64, ()>;

pub async fn check_usage_for_api_key(
    state: &UsageState,
    api_key: &AuthenticatedApiKey,
) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
    let organization_id = api_key.organization.id.0;
    let api_key_id = api_key.api_key.id.clone();

    debug!(
        "Checking usage limits for organization: {} and API key: {}",
        organization_id, api_key_id.0
    );

    // Only keys with a spend limit need the (expensive) spend aggregate. The
    // limit travels with the read so the per-key gate cannot be skipped by a
    // mismatch between the two.
    let api_key_spend = match api_key.api_key.spend_limit {
        Some(limit) => {
            let id = uuid::Uuid::parse_str(&api_key_id.0).map_err(|_| {
                tracing::error!("Failed to parse API key ID");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(ErrorResponse::new(
                        "Internal error".to_string(),
                        "internal_server_error".to_string(),
                    )),
                )
            })?;
            Some((limit, async move {
                state
                    .usage_repository
                    .get_api_key_spend(id)
                    .await
                    .map_err(|error| {
                        tracing::error!(error = %error, "Failed to get API key spend");
                    })
            }))
        }
        None => None,
    };

    run_usage_checks(
        state.staking_farm_service.as_ref(),
        state.usage_service.as_ref(),
        organization_id,
        api_key_spend,
    )
    .await
}

/// Run the pre-inference credit checks with as little sequential database
/// waiting as possible.
///
/// Order of operations:
/// 1. The per-key spend gate (keys with a spend limit only). It rejects
///    before any organization work, as it always did.
/// 2. The staking-farm preflight and the organization balance read run
///    concurrently: the preflight only ever writes the organization's
///    limits, never its balance.
/// 3. The spending limit is read after the preflight has finished, because
///    a stale staking source is synced there and the sync rewrites limits.
///    For organizations that have a staking source the balance is re-read
///    at this point too, so a preflight that waited on the NEAR RPC cannot
///    leave the gate evaluating a balance from before usage recorded in the
///    meantime. Organizations without a source (nearly all traffic) keep the
///    overlapped read.
///
/// On a cross-region replica each of these reads costs a full network round
/// trip, so overlapping them removes a round-trip wait from every request.
async fn run_usage_checks<F>(
    staking_farm_service: &(dyn StakingFarmPreflightSync + Send + Sync),
    usage_service: &(dyn UsageServiceTrait + Send + Sync),
    organization_id: uuid::Uuid,
    api_key_spend: Option<(i64, F)>,
) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)>
where
    F: Future<Output = ApiKeySpendRead>,
{
    if let Some((api_key_limit, spend)) = api_key_spend {
        let api_key_spend = spend.await.map_err(|()| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Failed to check API key spend".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

        if api_key_spend >= api_key_limit {
            warn!(
                "API key exceeded spend limit. Spent: {}, Limit: {}",
                format_amount(api_key_spend),
                format_amount(api_key_limit)
            );
            return Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    format!(
                        "API key spend limit exceeded. Spent: {}, Limit: {}",
                        format_amount(api_key_spend),
                        format_amount(api_key_limit)
                    ),
                    "api_key_limit_exceeded".to_string(),
                )),
            ));
        }

        debug!(
            "API key within spend limit. Spent: {}, Limit: {}, Remaining: {}",
            format_amount(api_key_spend),
            format_amount(api_key_limit),
            format_amount(api_key_limit - api_key_spend)
        );
    }

    let staking_preflight = async {
        match staking_farm_service
            .sync_organization_if_stale(organization_id)
            .await
        {
            Ok(source) => source.is_some(),
            Err(error) => {
                warn!(
                    organization_id = %organization_id,
                    error = %error,
                    "Staking farm preflight sync failed; continuing with last synced credits"
                );
                // Unknown whether a source exists; take the conservative path.
                true
            }
        }
    };

    let (has_staking_source, balance) = tokio::join!(
        staking_preflight,
        usage_service.get_balance(organization_id)
    );

    let usage_check_failed = |_| {
        tracing::error!("Failed to check usage limits");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorResponse::new(
                "Failed to check usage limits".to_string(),
                "internal_server_error".to_string(),
            )),
        )
    };
    let balance = if has_staking_source {
        // Same read order as before the overlap: balance after the preflight.
        usage_service.get_balance(organization_id).await
    } else {
        balance
    };
    let balance = balance.map_err(usage_check_failed)?;

    // Read limits only after the staking preflight above has completed.
    let limit = usage_service
        .get_limit(organization_id)
        .await
        .map_err(usage_check_failed)?;

    match UsageCheckResult::evaluate(balance.as_ref(), limit.as_ref()) {
        UsageCheckResult::Allowed { remaining } => {
            debug!(
                "Organization {} has sufficient credits. Remaining: {}",
                organization_id,
                format_amount(remaining)
            );
            Ok(())
        }
        UsageCheckResult::LimitExceeded { spent, limit } => {
            warn!(
                "Organization exceeded credit limit. Spent: {}, Limit: {}",
                format_amount(spent),
                format_amount(limit)
            );
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    format!(
                        "Credit limit exceeded. Spent: {}, Limit: {}. Please purchase more credits.",
                        format_amount(spent),
                        format_amount(limit)
                    ),
                    "insufficient_credits".to_string(),
                )),
            ))
        }
        UsageCheckResult::NoCredits => {
            warn!("Organization has no credits - denying request");
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    "No credits available. Please purchase credits to use the API.".to_string(),
                    "no_credits".to_string(),
                )),
            ))
        }
        UsageCheckResult::NoLimitSet => {
            warn!("Organization has no spending limit configured - denying request");
            Err((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    "No spending limit configured. Please contact support to set up credits."
                        .to_string(),
                    "no_limit_configured".to_string(),
                )),
            ))
        }
    }
}

/// Middleware to check if organization has sufficient credits before processing request
pub async fn usage_check_middleware(
    State(state): State<UsageState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<ErrorResponse>)> {
    let api_key = request
        .extensions()
        .get::<AuthenticatedApiKey>()
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new(
                    "API key authentication required".to_string(),
                    "unauthorized".to_string(),
                )),
            )
        })?;

    check_usage_for_api_key(&state, api_key).await?;
    Ok(next.run(request).await)
}

/// Spend-limit middleware for the native Anthropic routes, with the error
/// envelope Anthropic SDKs expect.
pub async fn anthropic_usage_check_middleware(
    State(state): State<UsageState>,
    request: Request,
    next: Next,
) -> Result<Response, (StatusCode, axum::Json<AnthropicErrorResponse>)> {
    let api_key = request
        .extensions()
        .get::<AuthenticatedApiKey>()
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(AnthropicErrorResponse::new(
                    "authentication_error",
                    "API key authentication required",
                )),
            )
        })?;

    check_usage_for_api_key(&state, api_key)
        .await
        .map_err(|(status, axum::Json(error))| {
            let error_type = if status.is_server_error() {
                "api_error"
            } else {
                "invalid_request_error"
            };
            (
                status,
                axum::Json(AnthropicErrorResponse::new(error_type, error.error.message)),
            )
        })?;
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::usage::{
        CostBreakdown, InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportQuery,
        InferenceUsageReportRow, OrganizationBalanceInfo, OrganizationCreditLimit,
        OrganizationLimit, RecordUsageApiRequest, RecordUsageServiceRequest, UsageByModelEntry,
        UsageError, UsageLogEntry,
    };
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Default)]
    struct MockStakingFarmPreflight {
        calls: Mutex<Vec<Uuid>>,
        should_fail: bool,
        /// Source the preflight reports for the organization, if any.
        source: Option<services::staking_farm::OrganizationStakingFarmSource>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    fn staking_source_fixture(
        organization_id: Uuid,
    ) -> services::staking_farm::OrganizationStakingFarmSource {
        let now = chrono::Utc::now();
        services::staking_farm::OrganizationStakingFarmSource {
            id: Uuid::new_v4(),
            organization_id,
            near_account_id: "alice.near".to_string(),
            network_id: "mainnet".to_string(),
            contract_id: "stake.dao".to_string(),
            farm_product_id: "prod_test".to_string(),
            farm_price_id: None,
            credit_nano_usd_per_reward_unit: 1,
            status: "active".to_string(),
            sync_status: "synced".to_string(),
            last_sync_error: None,
            created_by_user_id: None,
            created_at: now,
            updated_at: now,
            last_synced_at: Some(now),
            last_synced_accumulated_reward_units_24: None,
            last_synced_pending_reward_units_24: None,
            last_synced_reward_units_24: None,
            last_synced_credit_nano_usd: None,
            active_positions: serde_json::json!([]),
        }
    }

    impl StakingFarmPreflightSync for MockStakingFarmPreflight {
        fn sync_organization_if_stale(
            &self,
            organization_id: Uuid,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = anyhow::Result<
                            Option<services::staking_farm::OrganizationStakingFarmSource>,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            self.calls.lock().unwrap().push(organization_id);
            let events = self.events.clone();
            let should_fail = self.should_fail;
            let source = self.source.clone();
            Box::pin(async move {
                events.lock().unwrap().push("staking-start");
                // Let any concurrently running read make progress, so a limit
                // read that wrongly overlaps the preflight lands between
                // start and done.
                tokio::task::yield_now().await;
                events.lock().unwrap().push("staking-done");
                if should_fail {
                    anyhow::bail!("staking sync failed");
                }
                Ok(source)
            })
        }
    }

    struct MockUsageService {
        /// Balance the mock reports; `total_spent` in nano-dollars.
        total_spent: Option<i64>,
        /// Active spending limit the mock reports.
        spend_limit: Option<i64>,
        calls: Mutex<Vec<Uuid>>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl MockUsageService {
        fn allowed(events: Arc<Mutex<Vec<&'static str>>>) -> Self {
            Self {
                total_spent: Some(0),
                spend_limit: Some(1_000_000_000),
                calls: Mutex::new(Vec::new()),
                events,
            }
        }
    }

    #[async_trait::async_trait]
    impl UsageServiceTrait for MockUsageService {
        async fn calculate_cost(
            &self,
            _model_id: &str,
            _input_tokens: i32,
            _output_tokens: i32,
            _cache_read_tokens: i32,
        ) -> Result<CostBreakdown, UsageError> {
            unimplemented!()
        }

        async fn record_usage(
            &self,
            _request: RecordUsageServiceRequest,
        ) -> Result<UsageLogEntry, UsageError> {
            unimplemented!()
        }

        async fn record_usage_from_api(
            &self,
            _organization_id: Uuid,
            _workspace_id: Uuid,
            _api_key_id: Uuid,
            _request: RecordUsageApiRequest,
        ) -> Result<UsageLogEntry, UsageError> {
            unimplemented!()
        }

        async fn check_can_use(
            &self,
            _organization_id: Uuid,
        ) -> Result<UsageCheckResult, UsageError> {
            unimplemented!("the middleware reads balance and limit separately")
        }

        async fn get_balance(
            &self,
            organization_id: Uuid,
        ) -> Result<Option<OrganizationBalanceInfo>, UsageError> {
            self.events.lock().unwrap().push("balance");
            Ok(self.total_spent.map(|total_spent| OrganizationBalanceInfo {
                organization_id,
                total_spent,
                last_usage_at: None,
                total_requests: 1,
                total_tokens: 1,
                updated_at: chrono::Utc::now(),
            }))
        }

        async fn get_usage_history(
            &self,
            _organization_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
            unimplemented!()
        }

        async fn get_limit(
            &self,
            organization_id: Uuid,
        ) -> Result<Option<OrganizationLimit>, UsageError> {
            self.calls.lock().unwrap().push(organization_id);
            self.events.lock().unwrap().push("limits");
            Ok(self
                .spend_limit
                .map(|spend_limit| OrganizationLimit { spend_limit }))
        }

        async fn get_credit_limits(
            &self,
            _organization_id: Uuid,
        ) -> Result<Vec<OrganizationCreditLimit>, UsageError> {
            unimplemented!()
        }

        async fn get_usage_history_by_api_key(
            &self,
            _api_key_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
            unimplemented!()
        }

        async fn get_api_key_usage_history_with_permissions(
            &self,
            _workspace_id: Uuid,
            _api_key_id: Uuid,
            _user_id: Uuid,
            _limit: Option<i64>,
            _offset: Option<i64>,
        ) -> Result<(Vec<UsageLogEntry>, i64), UsageError> {
            unimplemented!()
        }

        async fn get_costs_by_inference_ids(
            &self,
            _organization_id: Uuid,
            _inference_ids: Vec<Uuid>,
        ) -> Result<Vec<InferenceCost>, UsageError> {
            unimplemented!()
        }

        async fn get_usage_by_model(
            &self,
            _organization_id: Uuid,
            _start_date: chrono::DateTime<chrono::Utc>,
        ) -> Result<Vec<UsageByModelEntry>, UsageError> {
            unimplemented!()
        }

        async fn list_inference_usage_report(
            &self,
            _query: InferenceUsageReportQuery,
        ) -> Result<Vec<InferenceUsageReportRow>, UsageError> {
            Ok(vec![])
        }

        async fn list_inference_usage_history(
            &self,
            _query: InferenceUsageHistoryQuery,
        ) -> Result<(Vec<InferenceUsageReportRow>, i64), UsageError> {
            Ok((vec![], 0))
        }
    }

    /// The staking preflight may rewrite limits, so the limit read must not
    /// start until the preflight has finished. Balance is independent and is
    /// allowed to overlap the preflight.
    fn assert_limits_read_after_staking(events: &[&str]) {
        let staking_done = events
            .iter()
            .position(|e| *e == "staking-done")
            .expect("staking ran");
        let limits = events
            .iter()
            .position(|e| *e == "limits")
            .expect("limits read");
        assert!(events.contains(&"balance"), "balance read: {events:?}");
        assert!(
            staking_done < limits,
            "limits read before staking preflight finished: {events:?}"
        );
    }

    /// Type for tests that pass no per-key spend read.
    type NoSpendRead = std::future::Ready<ApiKeySpendRead>;

    #[tokio::test]
    async fn staking_farm_preflight_runs_before_limit_read() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            source: None,
            events: events.clone(),
        };
        let usage = MockUsageService::allowed(events.clone());

        run_usage_checks::<NoSpendRead>(&staking, &usage, organization_id, None)
            .await
            .unwrap();

        assert_eq!(staking.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_eq!(usage.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_limits_read_after_staking(&events.lock().unwrap());
    }

    #[tokio::test]
    async fn staking_farm_preflight_failure_still_checks_usage() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: true,
            source: None,
            events: events.clone(),
        };
        let usage = MockUsageService::allowed(events.clone());

        run_usage_checks::<NoSpendRead>(&staking, &usage, organization_id, None)
            .await
            .unwrap();

        assert_eq!(staking.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_eq!(usage.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_limits_read_after_staking(&events.lock().unwrap());
    }

    #[tokio::test]
    async fn organization_over_limit_is_rejected_with_402() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            source: None,
            events: events.clone(),
        };
        let usage = MockUsageService {
            total_spent: Some(5_000_000_000),
            spend_limit: Some(1_000_000_000),
            calls: Mutex::new(Vec::new()),
            events: events.clone(),
        };

        let (status, axum::Json(error)) =
            run_usage_checks::<NoSpendRead>(&staking, &usage, organization_id, None)
                .await
                .unwrap_err();

        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(error.error.r#type, "insufficient_credits");
    }

    #[tokio::test]
    async fn api_key_over_its_spend_limit_is_rejected_before_org_checks() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            source: None,
            events: events.clone(),
        };
        let usage = MockUsageService::allowed(events.clone());

        let (status, axum::Json(error)) = run_usage_checks(
            &staking,
            &usage,
            organization_id,
            Some((10, async { Ok(10) })),
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(error.error.r#type, "api_key_limit_exceeded");
        // Nothing at the organization level runs once the key is over budget:
        // no staking preflight, no balance or limit read.
        assert!(staking.calls.lock().unwrap().is_empty());
        assert!(events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn balance_read_overlaps_the_staking_preflight_without_a_source() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            source: None,
            events: events.clone(),
        };
        let usage = MockUsageService::allowed(events.clone());

        run_usage_checks::<NoSpendRead>(&staking, &usage, organization_id, None)
            .await
            .unwrap();

        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["staking-start", "balance", "staking-done", "limits"],
            "balance must overlap the preflight and be read once"
        );
    }

    #[tokio::test]
    async fn balance_is_re_read_after_the_preflight_when_a_staking_source_exists() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            source: Some(staking_source_fixture(organization_id)),
            events: events.clone(),
        };
        let usage = MockUsageService::allowed(events.clone());

        run_usage_checks::<NoSpendRead>(&staking, &usage, organization_id, None)
            .await
            .unwrap();

        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "staking-start",
                "balance",
                "staking-done",
                "balance",
                "limits"
            ],
            "a staking organization must evaluate a balance read after the preflight"
        );
    }

    #[tokio::test]
    async fn api_key_spend_read_failure_is_a_500() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            source: None,
            events: events.clone(),
        };
        let usage = MockUsageService::allowed(events.clone());

        let (status, _) = run_usage_checks(
            &staking,
            &usage,
            organization_id,
            Some((10, async { Err(()) })),
        )
        .await
        .unwrap_err();

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
