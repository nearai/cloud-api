//! Monetary admission metadata. Reservations never contain request or response content.
use super::{BillingIdentity, InferenceType, ModelPricing, UsageError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Best-of sampling may execute more generations than the response returns.
pub fn generation_choices(
    n: Option<i64>,
    best_of: Option<&serde_json::Value>,
) -> Result<i32, UsageError> {
    let n = n.unwrap_or(1);
    let best = match best_of.filter(|value| !value.is_null()) {
        None => 1,
        Some(value) => value
            .as_i64()
            .filter(|n| *n > 0)
            .ok_or_else(|| UsageError::ValidationError("invalid best_of".into()))?,
    };
    if n <= 0 {
        return Err(UsageError::ValidationError("invalid n".into()));
    }
    i32::try_from(n.max(best))
        .map_err(|_| UsageError::CostCalculationOverflow("generation count".into()))
}

/// Limits enforced by the execution gateway and the selected model's context window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageBounds {
    pub inference_type: InferenceType,
    pub input_units: i32,
    pub output_tokens: i32,
    pub images: i32,
}

impl UsageBounds {
    pub fn text(context_length: i32, max_output: i32, choices: i32) -> Result<Self, UsageError> {
        if context_length <= 0 || max_output <= 0 || choices <= 0 {
            return Err(UsageError::ValidationError(
                "positive model and execution bounds are required".into(),
            ));
        }
        Ok(Self {
            inference_type: InferenceType::ChatCompletion,
            input_units: context_length
                .checked_mul(choices)
                .ok_or_else(|| UsageError::CostCalculationOverflow("input bound".into()))?,
            output_tokens: max_output
                .checked_mul(choices)
                .ok_or_else(|| UsageError::CostCalculationOverflow("output bound".into()))?,
            images: 0,
        })
    }

    pub fn quote(&self, pricing: &ModelPricing) -> Result<i64, UsageError> {
        if self.input_units < 0 || self.output_tokens < 0 || self.images < 0 {
            return Err(UsageError::ValidationError("negative usage bound".into()));
        }
        let product = |units: i32, rate: i64| -> Result<i64, UsageError> {
            if rate < 0 {
                return Err(UsageError::ValidationError("negative catalog price".into()));
            }
            i64::from(units)
                .checked_mul(rate)
                .ok_or_else(|| UsageError::CostCalculationOverflow("budget quote".into()))
        };
        match self.inference_type {
            InferenceType::ImageGeneration | InferenceType::ImageEdit => {
                product(self.images, pricing.cost_per_image)
            }
            InferenceType::AudioTranscription
            | InferenceType::Embedding
            | InferenceType::Rerank
            | InferenceType::Score
            | InferenceType::PrivacyClassify => {
                product(self.input_units, pricing.input_cost_per_token)
            }
            _ => {
                if let Some(profile) = &pricing.text_pricing {
                    return profile.maximum_cost(self.input_units, self.output_tokens);
                }
                // Legacy Anthropic cache writes may be charged at 1.25x. Reserving the largest
                // token-class rate also covers a provider cache miss or a cache-hit price increase.
                let rate = pricing
                    .input_cost_per_token
                    .max(pricing.cache_read_cost_per_token.unwrap_or(0))
                    .max(
                        super::five_minute_cache_write_rate(pricing.input_cost_per_token)
                            .ok_or_else(|| {
                                UsageError::CostCalculationOverflow("cache write quote".into())
                            })?,
                    );
                product(self.input_units, rate)?
                    .checked_add(product(self.output_tokens, pricing.output_cost_per_token)?)
                    .ok_or_else(|| UsageError::CostCalculationOverflow("budget total".into()))
            }
        }
    }

    pub fn validate_actual(
        &self,
        request: &super::RecordUsageServiceRequest,
    ) -> Result<(), UsageError> {
        if request.input_tokens < 0
            || request.output_tokens < 0
            || request.image_count.is_some_and(|n| n < 0)
            || request.input_tokens > self.input_units
            || request.output_tokens > self.output_tokens
            || request.image_count.unwrap_or(0) > self.images
        {
            return Err(UsageError::ValidationError(
                "reported usage exceeds admitted execution bounds".into(),
            ));
        }
        if request.input_tokens == 0
            && request.output_tokens == 0
            && request.image_count.unwrap_or(0) == 0
            && (self.input_units > 0 || self.output_tokens > 0 || self.images > 0)
        {
            return Err(UsageError::ValidationError(
                "final billable usage is missing; reservation retained".into(),
            ));
        }
        let compatible = request.inference_type == self.inference_type
            || matches!(
                (self.inference_type, request.inference_type),
                (
                    InferenceType::ChatCompletion,
                    InferenceType::ChatCompletionStream
                ) | (
                    InferenceType::ChatCompletionStream,
                    InferenceType::ChatCompletion
                )
            );
        if !compatible {
            return Err(UsageError::ValidationError(
                "usage kind differs from reservation".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BudgetReservation {
    pub id: Uuid,
    pub identity: BillingIdentity,
    pub amount: i64,
    pub pricing: ModelPricing,
    pub bounds: UsageBounds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub pricing: ModelPricing,
    pub bounds: UsageBounds,
}

use std::sync::Arc;
use tokio::sync::Mutex;

/// One logical operation can make several upstream attempts. Every attempt gets its own
/// reservation; failed attempts with unknown usage stay held independently of the winner.
#[derive(Clone)]
pub struct BudgetExecution {
    service: Arc<dyn super::UsageServiceTrait>,
    identity: BillingIdentity,
    bounds: UsageBounds,
    last: Arc<Mutex<Option<Uuid>>>,
}

tokio::task_local! { static EXECUTION: BudgetExecution; }

impl BudgetExecution {
    pub fn new(
        service: Arc<dyn super::UsageServiceTrait>,
        identity: BillingIdentity,
        bounds: UsageBounds,
    ) -> Self {
        Self {
            service,
            identity,
            bounds,
            last: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        EXECUTION
            .scope(
                self.clone(),
                inference_providers::retry_admission::scope(Arc::new(self.clone()), future),
            )
            .await
    }

    pub async fn id(&self) -> Result<Uuid, UsageError> {
        self.last
            .lock()
            .await
            .ok_or_else(|| UsageError::InternalError("execution has no admitted attempt".into()))
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
        .admit(execution.identity, execution.bounds.clone())
        .await?;
    *execution.last.lock().await = Some(id);
    Ok(())
}

pub fn completion_error(error: UsageError) -> inference_providers::CompletionError {
    inference_providers::CompletionError::HttpError {
        status_code: if matches!(error, UsageError::LimitExceeded(_)) {
            402
        } else {
            503
        },
        message: error.to_string(),
        is_external: false,
    }
}

pub fn service_error(error: UsageError) -> crate::completions::ports::CompletionError {
    crate::completions::ports::CompletionError::ProviderError {
        status_code: if matches!(error, UsageError::LimitExceeded(_)) {
            402
        } else {
            503
        },
        message: error.to_string(),
    }
}

#[async_trait::async_trait]
impl inference_providers::retry_admission::RetryAdmission for BudgetExecution {
    async fn admit_retry(&self) -> Result<(), inference_providers::CompletionError> {
        let id = self
            .service
            .admit(self.identity, self.bounds.clone())
            .await
            .map_err(completion_error)?;
        *self.last.lock().await = Some(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::CapturingUsageService;
    fn identity() -> BillingIdentity {
        BillingIdentity {
            organization_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            api_key_id: Uuid::new_v4(),
            model_id: Uuid::new_v4(),
        }
    }
    #[test]
    fn best_of_covers_hidden_generations() {
        assert_eq!(
            generation_choices(Some(2), Some(&serde_json::json!(10))).unwrap(),
            10
        );
        assert_eq!(
            generation_choices(Some(4), Some(&serde_json::json!(2))).unwrap(),
            4
        );
        assert!(generation_choices(Some(0), None).is_err());
        assert!(generation_choices(None, Some(&serde_json::json!(-1))).is_err());
        assert!(generation_choices(None, Some(&serde_json::json!(i64::MAX))).is_err());
    }
    #[tokio::test]
    async fn absent_admission_context_fails_closed() {
        assert!(before_attempt().await.is_err());
    }
    #[tokio::test]
    async fn retries_get_distinct_reservations_and_budget_exhaustion_stops_execution() {
        let mut service = CapturingUsageService::new();
        service.admission_limit = Some(2);
        let service = Arc::new(service);
        let execution = BudgetExecution::new(
            service.clone(),
            identity(),
            UsageBounds::text(100, 20, 1).unwrap(),
        );
        execution
            .run(async {
                before_attempt().await.unwrap();
                let first = execution.id().await.unwrap();
                before_attempt().await.unwrap();
                let second = execution.id().await.unwrap();
                assert_ne!(first, second);
                assert!(matches!(
                    before_attempt().await,
                    Err(UsageError::LimitExceeded(_))
                ));
                assert_eq!(service.admissions.lock().unwrap().len(), 2);
            })
            .await;
    }
    #[tokio::test]
    async fn provider_internal_retry_uses_another_reservation() {
        let mut service = CapturingUsageService::new();
        service.admission_limit = Some(2);
        let service = Arc::new(service);
        let execution = BudgetExecution::new(
            service.clone(),
            identity(),
            UsageBounds::text(100, 20, 1).unwrap(),
        );
        execution
            .run(async {
                before_attempt().await.unwrap();
                let first = execution.id().await.unwrap();
                inference_providers::retry_admission::before_retry()
                    .await
                    .unwrap();
                assert_ne!(first, execution.id().await.unwrap());
                assert!(inference_providers::retry_admission::before_retry()
                    .await
                    .is_err());
            })
            .await;
        assert_eq!(service.admissions.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancelled_execution_does_not_record_partial_usage() {
        let service = Arc::new(CapturingUsageService::new());
        let execution = BudgetExecution::new(
            service.clone(),
            identity(),
            UsageBounds::text(100, 20, 1).unwrap(),
        );
        let (sent, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            execution
                .run(async {
                    before_attempt().await.unwrap();
                    sent.send(()).unwrap();
                    std::future::pending::<()>().await;
                })
                .await
        });
        received.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(service.admissions.lock().unwrap().len(), 1);
        assert!(service.get_requests().is_empty());
    }
    #[test]
    fn bounds_reject_missing_excess_and_different_usage() {
        let bounds = UsageBounds::text(100, 20, 1).unwrap();
        let mut request = crate::usage::RecordUsageServiceRequest::base(
            identity(),
            InferenceType::ChatCompletion,
        );
        assert!(bounds.validate_actual(&request).is_err());
        request.input_tokens = 100;
        request.output_tokens = 20;
        assert!(bounds.validate_actual(&request).is_ok());
        for admitted in [
            InferenceType::ChatCompletion,
            InferenceType::ChatCompletionStream,
        ] {
            for reported in [
                InferenceType::ChatCompletion,
                InferenceType::ChatCompletionStream,
            ] {
                let mut bounds = bounds.clone();
                bounds.inference_type = admitted;
                request.inference_type = reported;
                assert!(bounds.validate_actual(&request).is_ok());
            }
        }
        request.output_tokens = 21;
        assert!(bounds.validate_actual(&request).is_err());
        request.output_tokens = 0;
        request.inference_type = InferenceType::ImageGeneration;
        assert!(bounds.validate_actual(&request).is_err());
    }
    #[test]
    fn quote_covers_cache_writes_multiple_choices_and_rejects_overflow() {
        let pricing = ModelPricing {
            id: Uuid::new_v4(),
            model_name: "test".into(),
            input_cost_per_token: 4,
            output_cost_per_token: 10,
            cache_read_cost_per_token: Some(1),
            cost_per_image: 0,
            text_pricing: None,
        };
        assert_eq!(
            UsageBounds::text(100, 20, 2)
                .unwrap()
                .quote(&pricing)
                .unwrap(),
            1400
        );
        assert!(UsageBounds::text(i32::MAX, 1, 2).is_err());
        let mut pricing = pricing;
        pricing.output_cost_per_token = i64::MAX;
        assert!(UsageBounds::text(1, 2, 1).unwrap().quote(&pricing).is_err());
    }
}
