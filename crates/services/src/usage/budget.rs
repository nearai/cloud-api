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

    pub async fn id(&self) -> Result<Option<Uuid>, UsageError> {
        self.last
            .lock()
            .await
            .map(|attempt| attempt.id)
            .ok_or_else(|| UsageError::InternalError("execution has no admitted attempt".into()))
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
    let id = execution
        .service
        .admit(execution.identity, execution.features.clone())
        .await?;
    *execution.last.lock().await = Some(Attempt {
        id,
        dispatched: false,
    });
    Ok(())
}

pub fn completion_error(error: UsageError) -> inference_providers::CompletionError {
    inference_providers::CompletionError::HttpError {
        status_code: admission_status(&error),
        message: error.to_string(),
        is_external: false,
    }
}

pub fn service_error(error: UsageError) -> crate::completions::ports::CompletionError {
    crate::completions::ports::CompletionError::ProviderError {
        status_code: admission_status(&error),
        message: error.to_string(),
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
            } else if self
                .service
                .release_budget(self.identity.organization_id, id)
                .await
                .is_err()
            {
                tracing::warn!(%id, "Undispatched budget release failed; awaiting expiry");
            }
        }
    }

    async fn release_not_dispatched(&self) {
        let attempt = self.last.lock().await.take();
        if let Some(Attempt { id: Some(id), .. }) = attempt {
            if self
                .service
                .release_budget(self.identity.organization_id, id)
                .await
                .is_err()
            {
                // Existing expiry remains the fallback; never turn release failure into a retry.
                tracing::warn!(%id, "Undispatched budget release failed; awaiting expiry");
            }
        }
    }
    async fn admit_retry(&self) -> Result<(), inference_providers::CompletionError> {
        self.attempt_failed().await;
        let id = self
            .service
            .admit(self.identity, self.features.clone())
            .await
            .map_err(completion_error)?;
        *self.last.lock().await = Some(Attempt {
            id,
            dispatched: false,
        });
        Ok(())
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
                matches!(completion_error(error.clone()), inference_providers::CompletionError::HttpError {status_code,..} if status_code==expected)
            );
            assert!(
                matches!(service_error(error), crate::completions::ports::CompletionError::ProviderError {status_code,..} if status_code==expected)
            );
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
