//! Monetary admission metadata, containing counts and prices only.
use super::{BillingIdentity, InferenceType, ModelPricing, UsageError};
use uuid::Uuid;

mod estimate;
pub(crate) mod metrics;
pub mod policy;
pub use estimate::UsageFeatures;
pub(super) use policy::ESTIMATOR_VERSION;

#[derive(Debug, serde::Serialize)]
pub struct BudgetAdmission {
    pub id: Option<Uuid>,
    pub mode: policy::AdmissionMode,
    pub estimated_amount: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct UsageEstimate {
    pub features: UsageFeatures,
    pub input_units: i32,
    pub output_tokens: i32,
    pub images: i32,
    policy: policy::EstimatePolicy,
}

impl UsageEstimate {
    pub fn quote(&self, pricing: &ModelPricing) -> Result<i64, UsageError> {
        let product = |units: i32, rate: i64| -> Result<i64, UsageError> {
            if units < 0 || rate < 0 {
                return Err(UsageError::ValidationError(
                    "negative billable units or price".into(),
                ));
            }
            i64::from(units).checked_mul(rate).ok_or_else(overflow)
        };
        let estimated = match self.features.inference_type {
            InferenceType::ImageGeneration | InferenceType::ImageEdit => {
                product(self.images, pricing.cost_per_image)?
            }
            InferenceType::AudioTranscription => {
                product(self.input_units, pricing.input_cost_per_token)?
            }
            _ => {
                if let Some(profile) = &pricing.text_pricing {
                    profile.admission_cost(
                        self.input_units,
                        self.output_tokens,
                        self.features.service_tier.as_deref(),
                    )?
                } else {
                    let rate = pricing
                        .input_cost_per_token
                        .max(pricing.cache_read_cost_per_token.unwrap_or(0))
                        .max(
                            super::five_minute_cache_write_rate(pricing.input_cost_per_token)
                                .ok_or_else(overflow)?,
                        );
                    product(self.input_units, rate)?
                        .checked_add(product(self.output_tokens, pricing.output_cost_per_token)?)
                        .ok_or_else(overflow)?
                }
            }
        };
        if estimated == 0 {
            return Ok(0);
        }
        let scaled = i128::from(estimated)
            .checked_mul(i128::from(self.policy.multiplier_basis_points))
            .and_then(|n| n.checked_add(9999))
            .ok_or_else(overflow)?
            / 10000;
        Ok(i64::try_from(scaled)
            .map_err(|_| overflow())?
            .max(self.policy.minimum_nano_usd))
    }
}

pub fn validate_actual(request: &super::RecordUsageServiceRequest) -> Result<(), UsageError> {
    // Estimates do not constrain valid actual usage, including receipts arriving after expiry.
    if request.input_tokens < 0
        || request.output_tokens < 0
        || request.image_count.is_some_and(|n| n < 0)
    {
        return Err(UsageError::ValidationError("negative actual usage".into()));
    }
    if request.input_tokens == 0
        && request.output_tokens == 0
        && request.image_count.unwrap_or(0) == 0
    {
        return Err(UsageError::ValidationError(
            "billable usage missing; reservation retained".into(),
        ));
    }
    Ok(())
}
fn overflow() -> UsageError {
    UsageError::CostCalculationOverflow("admission estimate overflow".into())
}

#[derive(Debug, Clone)]
pub struct BudgetReservation {
    pub id: Uuid,
    pub identity: BillingIdentity,
    pub amount: i64,
    pub inference_type: InferenceType,
    pub estimator_version: u32,
    pub organization_max_unsettled: i32,
    pub key_max_unsettled: i32,
    pub reservation_ttl_seconds: i32,
}

use std::sync::Arc;
use tokio::sync::Mutex;

/// One logical operation can make several upstream attempts. Every attempt gets its own
/// reservation; failed attempts with unknown usage stay held independently of the winner.
#[derive(Clone)]
pub struct BudgetExecution {
    service: Arc<dyn super::UsageServiceTrait>,
    identity: BillingIdentity,
    features: UsageFeatures,
    last: Arc<Mutex<Option<Attempt>>>,
}

#[derive(Clone, Copy)]
struct Attempt {
    id: Option<Uuid>,
    dispatched: bool,
}

tokio::task_local! { static EXECUTION: BudgetExecution; }

impl BudgetExecution {
    pub fn new(
        service: Arc<dyn super::UsageServiceTrait>,
        identity: BillingIdentity,
        features: UsageFeatures,
    ) -> Self {
        Self {
            service,
            identity,
            features,
            last: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        let mut guard = CancellationGuard(Some(self.clone()));
        let output = EXECUTION
            .scope(
                self.clone(),
                inference_providers::retry_admission::scope(Arc::new(self.clone()), future),
            )
            .await;
        guard.0 = None;
        output
    }

    async fn admit_attempt(&self) -> Result<(), UsageError> {
        let mut last = self.last.lock().await;
        let service = self.service.clone();
        let identity = self.identity;
        let features = self.features.clone();
        let mut pending = PendingAdmission {
            result: Some(Box::pin(
                async move { service.admit(identity, features).await },
            )),
            service: self.service.clone(),
            org: identity.organization_id,
        };
        let result = pending.result.as_mut().expect("pending admission").await;
        pending.result = None;
        *last = Some(Attempt {
            id: result?,
            dispatched: false,
        });
        Ok(())
    }

    pub async fn id(&self) -> Result<Option<Uuid>, UsageError> {
        self.last
            .lock()
            .await
            .map(|attempt| attempt.id)
            .ok_or_else(|| UsageError::InternalError("execution has no admitted attempt".into()))
    }
}

// If the caller leaves during admission, finish that same decision before releasing it.
struct PendingAdmission {
    result: Option<futures_util::future::BoxFuture<'static, Result<Option<Uuid>, UsageError>>>,
    service: Arc<dyn super::UsageServiceTrait>,
    org: Uuid,
}
impl Drop for PendingAdmission {
    fn drop(&mut self) {
        if let (Some(result), Ok(runtime)) =
            (self.result.take(), tokio::runtime::Handle::try_current())
        {
            let service = self.service.clone();
            let org = self.org;
            runtime.spawn(async move {
                if let Ok(Some(id)) = result.await {
                    release_undispatched(service.as_ref(), org, id).await;
                }
            });
        }
    }
}

async fn release_undispatched(service: &dyn super::UsageServiceTrait, org: Uuid, id: Uuid) {
    if service.release_budget(org, id).await.is_err() {
        unknown_outcome(service, org, Some(id)).await;
    }
}

/// Cancellation/panic before a response has been handed to its existing usage observer.
struct CancellationGuard(Option<BudgetExecution>);
impl Drop for CancellationGuard {
    fn drop(&mut self) {
        if let (Some(execution), Ok(runtime)) =
            (self.0.take(), tokio::runtime::Handle::try_current())
        {
            runtime.spawn(async move {
                use inference_providers::retry_admission::RetryAdmission;
                execution.attempt_failed().await;
            });
        }
    }
}

/// Best-effort lifecycle update only; never charge estimated usage or retry inference.
pub async fn unknown_outcome(
    service: &dyn super::UsageServiceTrait,
    organization_id: Uuid,
    id: Option<Uuid>,
) {
    if let Some(id) = id {
        if service
            .mark_budget_unknown(organization_id, id)
            .await
            .is_err()
        {
            tracing::warn!(%id, "Budget outcome update failed; awaiting expiry");
        }
    }
}

/// Covers response-body parsing/cancellation until an existing usage observer owns completion.
pub struct OutcomeGuard {
    service: Arc<dyn super::UsageServiceTrait>,
    organization_id: Uuid,
    id: Option<Uuid>,
}
impl OutcomeGuard {
    pub fn new(
        service: Arc<dyn super::UsageServiceTrait>,
        organization_id: Uuid,
        id: Option<Uuid>,
    ) -> Self {
        Self {
            service,
            organization_id,
            id,
        }
    }
    pub fn disarm(&mut self) {
        self.id = None;
    }
}
impl Drop for OutcomeGuard {
    fn drop(&mut self) {
        spawn_unknown_outcome(self.service.clone(), self.organization_id, self.id);
    }
}

pub fn spawn_unknown_outcome(
    service: Arc<dyn super::UsageServiceTrait>,
    organization_id: Uuid,
    id: Option<Uuid>,
) {
    if id.is_some() {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                unknown_outcome(service.as_ref(), organization_id, id).await;
            });
        }
    }
}

/// Called immediately before each provider dispatch, including retries. No request-content
/// or HTTP task-local state escapes into background streaming/settlement work.
pub async fn before_attempt() -> Result<(), UsageError> {
    let execution = EXECUTION
        .try_with(Clone::clone)
        .map_err(|_| UsageError::InternalError("execution is missing monetary admission".into()))?;
    execution.admit_attempt().await
}

pub fn completion_error(error: UsageError) -> inference_providers::CompletionError {
    inference_providers::CompletionError::Admission {
        status_code: admission_status(&error),
        message: public_message(&error),
    }
}

pub fn service_error(error: UsageError) -> crate::completions::ports::CompletionError {
    crate::completions::ports::CompletionError::Admission {
        status_code: admission_status(&error),
        message: public_message(&error),
    }
}

const UNAVAILABLE_MESSAGE: &str = "Monetary admission unavailable";

pub(crate) fn public_message(error: &UsageError) -> String {
    if admission_status(error) == 503 {
        tracing::warn!(
            reason = metrics::error_reason(error),
            "Monetary admission unavailable"
        );
        UNAVAILABLE_MESSAGE.into()
    } else {
        error.to_string()
    }
}

pub fn admission_error_type(status: u16, message: &str) -> &'static str {
    match status {
        400 => "invalid_request_error",
        402 if message.starts_with("Limit exceeded: API key spend limit exceeded.") => {
            "api_key_limit_exceeded"
        }
        402 => "insufficient_credits",
        403 => "permission_error",
        429 => "rate_limit_exceeded",
        _ => "service_overloaded",
    }
}

pub fn admission_status(error: &UsageError) -> u16 {
    match error {
        UsageError::LimitExceeded(_) => 402,
        UsageError::CapacityExceeded => 429,
        UsageError::ValidationError(_) | UsageError::CostCalculationOverflow(_) => 400,
        UsageError::Unauthorized(_) => 403,
        _ => 503,
    }
}

#[async_trait::async_trait]
impl inference_providers::retry_admission::RetryAdmission for BudgetExecution {
    async fn mark_dispatched(&self) {
        if let Some(attempt) = self.last.lock().await.as_mut() {
            attempt.dispatched = true;
        }
    }

    async fn attempt_failed(&self) {
        let attempt = self.last.lock().await.take();
        if let Some(Attempt {
            id: Some(id),
            dispatched,
        }) = attempt
        {
            if dispatched {
                unknown_outcome(
                    self.service.as_ref(),
                    self.identity.organization_id,
                    Some(id),
                )
                .await;
            } else {
                release_undispatched(self.service.as_ref(), self.identity.organization_id, id)
                    .await;
            }
        }
    }

    async fn release_not_dispatched(&self) {
        let attempt = self.last.lock().await.take();
        if let Some(Attempt { id: Some(id), .. }) = attempt {
            release_undispatched(self.service.as_ref(), self.identity.organization_id, id).await;
        }
    }
    async fn admit_retry(&self) -> Result<(), inference_providers::CompletionError> {
        self.attempt_failed().await;
        self.admit_attempt().await.map_err(completion_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn admission_errors_preserve_client_statuses() {
        for (error, expected) in [
            (UsageError::CapacityExceeded, 429),
            (UsageError::LimitExceeded("budget".into()), 402),
            (UsageError::ValidationError("count".into()), 400),
            (UsageError::CostCalculationOverflow("count".into()), 400),
            (UsageError::InternalError("database".into()), 503),
        ] {
            assert_eq!(admission_status(&error), expected);
            assert!(
                matches!(completion_error(error.clone()), inference_providers::CompletionError::Admission {status_code,..} if status_code==expected)
            );
            assert!(
                matches!(service_error(error), crate::completions::ports::CompletionError::Admission {status_code,..} if status_code==expected)
            );
        }
    }

    #[test]
    fn admission_unavailability_does_not_expose_repository_details() {
        let error = UsageError::InternalError("host=private-db SELECT secret FROM account".into());
        for message in [
            match completion_error(error.clone()) {
                inference_providers::CompletionError::Admission {
                    status_code: 503,
                    message,
                    ..
                } => message,
                other => panic!("{other:?}"),
            },
            match service_error(error) {
                crate::completions::ports::CompletionError::Admission {
                    status_code: 503,
                    message,
                } => message,
                other => panic!("{other:?}"),
            },
        ] {
            assert_eq!(message, "Monetary admission unavailable");
        }
    }

    #[test]
    fn quote_rounds_up_uses_minimum_and_preserves_free_models_without_overflow() {
        let pricing = ModelPricing {
            id: Uuid::new_v4(),
            model_name: "test".into(),
            input_cost_per_token: 3,
            output_cost_per_token: 7,
            cost_per_image: 11,
            cache_read_cost_per_token: Some(0),
            text_pricing: None,
        };
        for (kind, body, expected) in [
            (InferenceType::ImageGeneration, json!({"n":3}), 50),
            (InferenceType::ImageEdit, json!({"n":1}), 17),
            (InferenceType::AudioTranscription, json!({}), 270),
        ] {
            let mut estimate = UsageFeatures::from_json(kind, &body)
                .unwrap()
                .estimate()
                .unwrap();
            estimate.policy.minimum_nano_usd = 1;
            estimate.policy.multiplier_basis_points = 15_000;
            assert_eq!(estimate.quote(&pricing).unwrap(), expected);
        }
        let estimate = UsageFeatures::from_json(InferenceType::ImageGeneration, &json!({}))
            .unwrap()
            .estimate()
            .unwrap();
        assert_eq!(estimate.quote(&pricing).unwrap(), 1000);
        let mut free = pricing.clone();
        free.cost_per_image = 0;
        assert_eq!(estimate.quote(&free).unwrap(), 0);
        let mut huge = pricing;
        huge.cost_per_image = i64::MAX;
        assert!(estimate.quote(&huge).is_err());
    }

    #[tokio::test]
    async fn cancelled_admission_finishes_then_releases_its_own_hold() {
        use inference_providers::retry_admission::RetryAdmission;
        use tokio::time::{timeout, Duration};
        for retry in [false, true] {
            let mut service = crate::test_utils::CapturingUsageService::new();
            service.admission_reply = Some(tokio::sync::Semaphore::new(usize::from(retry)));
            service.fail_release = retry;
            let service = Arc::new(service);
            let execution = BudgetExecution::new(
                service.clone(),
                BillingIdentity {
                    organization_id: Uuid::new_v4(),
                    workspace_id: Uuid::new_v4(),
                    api_key_id: Uuid::new_v4(),
                    model_id: Uuid::new_v4(),
                },
                UsageFeatures::from_json(InferenceType::ChatCompletion, &json!({})).unwrap(),
            );
            let task = tokio::spawn(async move {
                execution
                    .run(async {
                        before_attempt().await.unwrap();
                        if retry {
                            execution.mark_dispatched().await;
                            execution.admit_retry().await.unwrap();
                        }
                    })
                    .await;
            });
            timeout(Duration::from_secs(2), async {
                while service.admissions.lock().unwrap().len() != 1 + usize::from(retry) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(service.releases.lock().unwrap().is_empty());
            service.admission_reply.as_ref().unwrap().add_permits(1);
            timeout(Duration::from_secs(2), async {
                while service.releases.lock().unwrap().is_empty()
                    || (retry && service.unknown_outcomes.lock().unwrap().len() < 2)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let admissions = service.admissions.lock().unwrap();
            assert_eq!(
                *service.releases.lock().unwrap(),
                [*admissions.last().unwrap()]
            );
            if retry {
                assert_eq!(*service.unknown_outcomes.lock().unwrap(), *admissions);
            } else {
                assert!(service.unknown_outcomes.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn cancellation_releases_pre_dispatch_and_marks_dispatched_attempt_unknown() {
        use inference_providers::retry_admission::RetryAdmission;
        for dispatched in [false, true] {
            let service = Arc::new(crate::test_utils::CapturingUsageService::new());
            let execution = BudgetExecution::new(
                service.clone(),
                BillingIdentity {
                    organization_id: Uuid::new_v4(),
                    workspace_id: Uuid::new_v4(),
                    api_key_id: Uuid::new_v4(),
                    model_id: Uuid::new_v4(),
                },
                UsageFeatures::from_json(InferenceType::ChatCompletion, &json!({"max_tokens":1}))
                    .unwrap(),
            );
            let (ready, started) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                execution
                    .run(async {
                        before_attempt().await.unwrap();
                        if dispatched {
                            execution.mark_dispatched().await;
                        }
                        ready.send(()).unwrap();
                        std::future::pending::<()>().await;
                    })
                    .await;
            });
            started.await.unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if service.releases.lock().unwrap().len()
                        + service.unknown_outcomes.lock().unwrap().len()
                        == 1
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                service.releases.lock().unwrap().len(),
                usize::from(!dispatched)
            );
            assert_eq!(
                service.unknown_outcomes.lock().unwrap().len(),
                usize::from(dispatched)
            );
        }
    }

    #[tokio::test]
    async fn completed_inner_timeout_retains_funding_and_marks_unknown() {
        use inference_providers::retry_admission::RetryAdmission;
        let service = Arc::new(crate::test_utils::CapturingUsageService::new());
        let identity = BillingIdentity {
            organization_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            api_key_id: Uuid::new_v4(),
            model_id: Uuid::new_v4(),
        };
        let execution = BudgetExecution::new(
            service.clone(),
            identity,
            UsageFeatures::measure_json(InferenceType::AudioTranscription, &json!({})),
        );
        let result = execution
            .run(tokio::time::timeout(
                std::time::Duration::from_millis(5),
                async {
                    before_attempt().await.unwrap();
                    execution.mark_dispatched().await;
                    std::future::pending::<()>().await;
                },
            ))
            .await;
        assert!(result.is_err());
        let id = execution.id().await.unwrap().unwrap();
        drop(OutcomeGuard::new(
            service.clone(),
            identity.organization_id,
            Some(id),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while service.unknown_outcomes.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*service.unknown_outcomes.lock().unwrap(), vec![id]);
        assert!(service.releases.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn provider_retry_gets_a_new_hold_and_does_not_reuse_an_unknown_attempt() {
        use inference_providers::retry_admission::RetryAdmission;
        let service = Arc::new(crate::test_utils::CapturingUsageService::new());
        let execution = BudgetExecution::new(
            service.clone(),
            BillingIdentity {
                organization_id: Uuid::new_v4(),
                workspace_id: Uuid::new_v4(),
                api_key_id: Uuid::new_v4(),
                model_id: Uuid::new_v4(),
            },
            UsageFeatures::from_json(InferenceType::ChatCompletion, &json!({"max_tokens":10}))
                .unwrap(),
        );
        assert!(execution.id().await.is_err());
        execution
            .run(async {
                before_attempt().await.unwrap();
                let first = execution.id().await.unwrap();
                execution.mark_dispatched().await;
                inference_providers::retry_admission::attempt_failed().await;
                assert!(execution.id().await.is_err());
                assert_eq!(
                    *service.unknown_outcomes.lock().unwrap(),
                    vec![first.unwrap()]
                );
                inference_providers::retry_admission::before_retry()
                    .await
                    .unwrap();
                let second = execution.id().await.unwrap();
                assert_ne!(first, second);
                assert!(service.releases.lock().unwrap().is_empty());
                // Local preparation failed before transport; release once and recover the slot.
                inference_providers::retry_admission::attempt_failed().await;
                inference_providers::retry_admission::attempt_failed().await;
                assert!(execution.id().await.is_err());
                assert_eq!(*service.releases.lock().unwrap(), vec![second.unwrap()]);
                for _ in 0..20 {
                    before_attempt().await.unwrap();
                    inference_providers::retry_admission::attempt_failed().await;
                }
                let released = service.releases.lock().unwrap();
                assert_eq!(released.len(), 21);
                assert!(
                    !released.contains(&first.unwrap()),
                    "unknown outcome must retain its hold"
                );
            })
            .await;
    }
}
