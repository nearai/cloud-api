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
use crate::models::ErrorResponse;
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

    // First, check API key spend limit if one is set
    if let Some(api_key_limit) = api_key.api_key.spend_limit {
        let api_key_uuid = uuid::Uuid::parse_str(&api_key_id.0).map_err(|_| {
            tracing::error!("Failed to parse API key ID");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(ErrorResponse::new(
                    "Internal error".to_string(),
                    "internal_server_error".to_string(),
                )),
            )
        })?;

        let api_key_spend = state
            .usage_repository
            .get_api_key_spend(api_key_uuid)
            .await
            .map_err(|_| {
                tracing::error!("Failed to get API key spend");
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
            "API key {} within spend limit. Spent: {}, Limit: {}, Remaining: {}",
            api_key_id.0,
            format_amount(api_key_spend),
            format_amount(api_key_limit),
            format_amount(api_key_limit - api_key_spend)
        );
    }

    check_organization_usage_after_staking_preflight(
        state.staking_farm_service.as_ref(),
        state.usage_service.as_ref(),
        organization_id,
    )
    .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use services::usage::{
        CostBreakdown, InferenceCost, OrganizationBalanceInfo, OrganizationCreditLimit,
        OrganizationLimit, RecordUsageApiRequest, RecordUsageServiceRequest, UsageByModelEntry,
        UsageError, UsageLogEntry,
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
}
