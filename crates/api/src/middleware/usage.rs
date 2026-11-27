use axum::{
    extract::{Request, State},
    http::{header::RETRY_AFTER, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};
use services::usage::{UsageCheckResult, UsageServiceTrait};
use std::{future::Future, pin::Pin, sync::Arc};
use tracing::{debug, warn};

use super::auth::AuthenticatedApiKey;
use crate::models::{AnthropicErrorResponse, ErrorResponse};
use crate::routes::common::format_amount;

/// A rejected usage check: status, headers (`Retry-After` on a 429) and body.
pub type UsageRejection = (StatusCode, HeaderMap, axum::Json<ErrorResponse>);

fn without_headers((status, body): (StatusCode, axum::Json<ErrorResponse>)) -> UsageRejection {
    (status, HeaderMap::new(), body)
}

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
    /// Answers the API key spend-limit check (see `services::api_key_spend`).
    pub spend_cache: services::api_key_spend::ApiKeySpendCache,
    pub api_key_repository: Arc<database::repositories::ApiKeyRepository>,
}

pub async fn check_usage_for_api_key(
    state: &UsageState,
    api_key: &AuthenticatedApiKey,
) -> Result<(), UsageRejection> {
    let organization_id = api_key.organization.id.0;
    let api_key_id = api_key.api_key.id.clone();

    debug!(
        "Checking usage limits for organization: {} and API key: {}",
        organization_id, api_key_id.0
    );

    // First, check API key spend limit if one is set
    if let Some(api_key_limit) = api_key.api_key.spend_limit {
        check_api_key_spend_limit(&state.spend_cache, &api_key_id.0, api_key_limit).await?;
    }

    check_organization_usage_after_staking_preflight(
        state.staking_farm_service.as_ref(),
        state.usage_service.as_ref(),
        organization_id,
    )
    .await
    .map_err(without_headers)
}

/// Rejects with 402 when the key's spend has reached `limit`, and with 429
/// plus `Retry-After` when the local budget has no headroom left for this
/// process until its next reading (`services::api_key_spend`). The spend can
/// lag the database by the budget's overshoot bound. A failed read is a 500.
async fn check_api_key_spend_limit(
    spend_cache: &services::api_key_spend::ApiKeySpendCache,
    api_key_id: &str,
    api_key_limit: i64,
) -> Result<(), UsageRejection> {
    use services::api_key_spend::Decision;

    let api_key_uuid = uuid::Uuid::parse_str(api_key_id).map_err(|_| {
        tracing::error!("Failed to parse API key ID");
        without_headers((
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(ErrorResponse::new(
                "Internal error".to_string(),
                "internal_server_error".to_string(),
            )),
        ))
    })?;

    let decision = spend_cache
        .check(api_key_uuid, api_key_limit)
        .await
        .map_err(|_| {
            tracing::error!("Failed to get API key spend");
            without_headers((
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Failed to check API key spend".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        })?;

    match decision {
        Decision::OverLimit {
            spend: api_key_spend,
        } => {
            warn!(
                "API key exceeded spend limit. Spent: {}, Limit: {}",
                format_amount(api_key_spend),
                format_amount(api_key_limit)
            );
            Err(without_headers((
                StatusCode::PAYMENT_REQUIRED,
                axum::Json(ErrorResponse::new(
                    format!(
                        "API key spend limit exceeded. Spent: {}, Limit: {}",
                        format_amount(api_key_spend),
                        format_amount(api_key_limit)
                    ),
                    "api_key_limit_exceeded".to_string(),
                )),
            )))
        }
        Decision::Throttle { retry_after_secs } => {
            debug!(retry_after_secs, "API key spend budget used up for now");
            let mut headers = HeaderMap::new();
            headers.insert(RETRY_AFTER, HeaderValue::from(retry_after_secs));
            Err((
                StatusCode::TOO_MANY_REQUESTS,
                headers,
                axum::Json(ErrorResponse::new(
                    format!(
                        "API key is close to its spend limit. Try again in {retry_after_secs} seconds."
                    ),
                    "rate_limit_exceeded".to_string(),
                )),
            ))
        }
        Decision::Admit => {
            debug!("API key within spend limit");
            Ok(())
        }
    }
}

async fn check_organization_usage_after_staking_preflight(
    staking_farm_service: &(dyn StakingFarmPreflightSync + Send + Sync),
    usage_service: &(dyn UsageServiceTrait + Send + Sync),
    organization_id: uuid::Uuid,
) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
    if let Err(error) = staking_farm_service
        .sync_organization_if_stale(organization_id)
        .await
    {
        warn!(
            organization_id = %organization_id,
            error = %error,
            "Staking farm preflight sync failed; continuing with last synced credits"
        );
    }

    // Check if organization can make request
    let check_result = usage_service
        .check_can_use(organization_id)
        .await
        .map_err(|_| {
            tracing::error!("Failed to check usage limits");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Failed to check usage limits".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

    match check_result {
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
) -> Result<Response, UsageRejection> {
    let api_key = request
        .extensions()
        .get::<AuthenticatedApiKey>()
        .ok_or_else(|| {
            without_headers((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new(
                    "API key authentication required".to_string(),
                    "unauthorized".to_string(),
                )),
            ))
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
) -> Result<Response, (StatusCode, HeaderMap, axum::Json<AnthropicErrorResponse>)> {
    let api_key = request
        .extensions()
        .get::<AuthenticatedApiKey>()
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                HeaderMap::new(),
                axum::Json(AnthropicErrorResponse::new(
                    "authentication_error",
                    "API key authentication required",
                )),
            )
        })?;

    check_usage_for_api_key(&state, api_key).await.map_err(
        |(status, headers, axum::Json(error))| {
            let error_type = if status.is_server_error() {
                "api_error"
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                "rate_limit_error"
            } else {
                "invalid_request_error"
            };
            (
                status,
                headers,
                axum::Json(AnthropicErrorResponse::new(error_type, error.error.message)),
            )
        },
    )?;
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use services::usage::{
        InferenceCost, InferenceUsageHistoryQuery, InferenceUsageReportRow,
        OrganizationBalanceInfo, OrganizationCreditLimit, OrganizationLimit, RecordUsageApiRequest,
        RecordUsageServiceRequest, UsageByModelEntry, UsageError, UsageLogEntry,
    };
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Default)]
    struct MockStakingFarmPreflight {
        calls: Mutex<Vec<Uuid>>,
        should_fail: bool,
        events: Arc<Mutex<Vec<&'static str>>>,
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
            self.events.lock().unwrap().push("staking");
            let should_fail = self.should_fail;
            Box::pin(async move {
                if should_fail {
                    anyhow::bail!("staking sync failed");
                }
                Ok(None)
            })
        }
    }

    struct MockUsageService {
        result: UsageCheckResult,
        calls: Mutex<Vec<Uuid>>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait::async_trait]
    impl UsageServiceTrait for MockUsageService {
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
            _discount: Option<services::usage::UsageDiscount>,
            _admission_id: Option<Uuid>,
        ) -> Result<UsageLogEntry, UsageError> {
            unimplemented!()
        }

        async fn check_can_use(
            &self,
            organization_id: Uuid,
        ) -> Result<UsageCheckResult, UsageError> {
            self.calls.lock().unwrap().push(organization_id);
            self.events.lock().unwrap().push("usage");
            Ok(self.result.clone())
        }

        async fn get_balance(
            &self,
            _organization_id: Uuid,
        ) -> Result<Option<OrganizationBalanceInfo>, UsageError> {
            unimplemented!()
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
            _organization_id: Uuid,
        ) -> Result<Option<OrganizationLimit>, UsageError> {
            unimplemented!()
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
            _credit_type: Option<&str>,
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
            _credit_type: Option<&str>,
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
            _start_date: jiff::Timestamp,
            _end_date: Option<jiff::Timestamp>,
        ) -> Result<Vec<UsageByModelEntry>, UsageError> {
            unimplemented!()
        }

        async fn list_inference_usage_history(
            &self,
            _query: InferenceUsageHistoryQuery,
        ) -> Result<(Vec<InferenceUsageReportRow>, i64), UsageError> {
            Ok((vec![], 0))
        }
    }

    #[tokio::test]
    async fn staking_farm_preflight_runs_before_usage_check() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: false,
            events: events.clone(),
        };
        let usage = MockUsageService {
            result: UsageCheckResult::Allowed {
                remaining: 1_000_000_000,
            },
            calls: Mutex::new(Vec::new()),
            events: events.clone(),
        };

        check_organization_usage_after_staking_preflight(&staking, &usage, organization_id)
            .await
            .unwrap();

        assert_eq!(staking.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_eq!(usage.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_eq!(events.lock().unwrap().as_slice(), &["staking", "usage"]);
    }

    #[tokio::test]
    async fn staking_farm_preflight_failure_still_checks_usage() {
        let organization_id = Uuid::new_v4();
        let events = Arc::new(Mutex::new(Vec::new()));
        let staking = MockStakingFarmPreflight {
            calls: Mutex::new(Vec::new()),
            should_fail: true,
            events: events.clone(),
        };
        let usage = MockUsageService {
            result: UsageCheckResult::Allowed {
                remaining: 1_000_000_000,
            },
            calls: Mutex::new(Vec::new()),
            events: events.clone(),
        };

        check_organization_usage_after_staking_preflight(&staking, &usage, organization_id)
            .await
            .unwrap();

        assert_eq!(staking.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_eq!(usage.calls.lock().unwrap().as_slice(), &[organization_id]);
        assert_eq!(events.lock().unwrap().as_slice(), &["staking", "usage"]);
    }

    struct FixedSpend(Result<i64, ()>);

    #[async_trait::async_trait]
    impl services::api_key_spend::ports::ApiKeySpendSource for FixedSpend {
        async fn api_key_spend(&self, _id: Uuid) -> anyhow::Result<i64> {
            self.0.map_err(|_| anyhow::anyhow!("database down"))
        }
    }

    /// The budget is on when `budget` is true, and off otherwise.
    fn spend_cache(
        spend: Result<i64, ()>,
        budget: bool,
    ) -> services::api_key_spend::ApiKeySpendCache {
        services::api_key_spend::ApiKeySpendCache::new(
            Arc::new(FixedSpend(spend)),
            Arc::new(services::metrics::capturing::CapturingMetricsService::new()),
            Arc::new(services::api_key_spend::ports::SystemClock),
            budget.then_some(60),
        )
    }

    /// The 402 body is the one clients saw before the budget, for a spend at
    /// the limit and over it, with the budget on and off.
    #[tokio::test]
    async fn spend_at_or_over_the_limit_is_402_with_the_same_body() {
        let id = Uuid::new_v4().to_string();
        for budget in [false, true] {
            for spend in [5_000_000_000, 7_500_000_000] {
                let cache = spend_cache(Ok(spend), budget);
                let (status, headers, axum::Json(body)) =
                    check_api_key_spend_limit(&cache, &id, 5_000_000_000)
                        .await
                        .unwrap_err();
                assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
                assert!(headers.get(RETRY_AFTER).is_none());
                assert_eq!(body.error.r#type, "api_key_limit_exceeded");
                assert_eq!(
                    body.error.message,
                    format!(
                        "API key spend limit exceeded. Spent: {}, Limit: {}",
                        format_amount(spend),
                        format_amount(5_000_000_000)
                    )
                );
            }
        }
    }

    #[tokio::test]
    async fn spend_under_the_limit_passes_and_a_failed_read_is_a_500() {
        let id = Uuid::new_v4().to_string();
        for budget in [false, true] {
            check_api_key_spend_limit(&spend_cache(Ok(4_999_999_999), budget), &id, 5_000_000_000)
                .await
                .unwrap();
            let (status, _, axum::Json(body)) =
                check_api_key_spend_limit(&spend_cache(Err(()), budget), &id, 5_000_000_000)
                    .await
                    .unwrap_err();
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body.error.message, "Failed to check API key spend");
        }
        let (status, _, _) = check_api_key_spend_limit(&spend_cache(Ok(0), true), "not-a-uuid", 1)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn budget_throttle_is_429_with_retry_after() {
        let key = Uuid::new_v4();
        // Spend 950 of 1000 is inside the 5% band: the slice is 12 (25% of
        // the 50 left), and using it leaves 38.
        let cache = spend_cache(Ok(950), true);
        let id = key.to_string();
        check_api_key_spend_limit(&cache, &id, 1_000).await.unwrap();
        cache.record_cost(key, 12).await;
        let (status, headers, axum::Json(body)) = check_api_key_spend_limit(&cache, &id, 1_000)
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = headers
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .expect("Retry-After in whole seconds");
        assert!(retry_after >= 1);
        assert_eq!(body.error.r#type, "rate_limit_exceeded");
        assert!(body.error.message.contains(&retry_after.to_string()));
    }
}
