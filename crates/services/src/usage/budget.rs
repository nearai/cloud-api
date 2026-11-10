//! Monetary admission metadata, containing counts and prices only.
use super::{BillingIdentity, InferenceType, ModelPricing, UsageError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod estimate;
pub use estimate::UsageFeatures;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEstimate {
    pub features: UsageFeatures,
    pub input_units: i32,
    pub output_tokens: i32,
    pub images: i32,
    pub policy: config::admission::EstimatePolicy,
    pub config_version: u32,
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
                    profile.maximum_cost(self.input_units, self.output_tokens)?
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

    pub fn validate_actual(
        &self,
        request: &super::RecordUsageServiceRequest,
    ) -> Result<(), UsageError> {
        // Estimates do not constrain valid actual usage. Unknown usage never releases a hold.
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
        let kind = self.features.inference_type;
        let compatible = request.inference_type == kind
            || matches!(
                (kind, request.inference_type),
                (
                    InferenceType::ChatCompletion,
                    InferenceType::ChatCompletionStream
                ) | (
                    InferenceType::ChatCompletionStream,
                    InferenceType::ChatCompletion
                ) | (InferenceType::ImageEdit, InferenceType::ImageGeneration)
            );
        if !compatible {
            return Err(UsageError::ValidationError(
                "usage kind differs from admission".into(),
            ));
        }
        Ok(())
    }
}

fn overflow() -> UsageError {
    UsageError::CostCalculationOverflow("admission estimate overflow".into())
}

#[derive(Debug, Clone)]
pub struct BudgetReservation {
    pub id: Uuid,
    pub identity: BillingIdentity,
    pub amount: i64,
    pub pricing: ModelPricing,
    pub estimate: UsageEstimate,
    pub organization_max_unsettled: i32,
    pub key_max_unsettled: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub pricing: ModelPricing,
    pub estimate: UsageEstimate,
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
    last: Arc<Mutex<Option<Uuid>>>,
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
        .admit(execution.identity, execution.features.clone())
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
            .admit(self.identity, self.features.clone())
            .await
            .map_err(completion_error)?;
        *self.last.lock().await = Some(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let mut config = config::admission::AdmissionConfig::default();
        config.default.minimum_nano_usd = 1;
        config.default.multiplier_basis_points = 15_000;
        for (kind, body, expected) in [
            (InferenceType::ImageGeneration, json!({"n":3}), 50),
            (InferenceType::ImageEdit, json!({"n":1}), 17),
            (InferenceType::AudioTranscription, json!({}), 270),
        ] {
            let estimate = UsageFeatures::from_json(kind, &body)
                .unwrap()
                .estimate(&config, "test")
                .unwrap();
            assert_eq!(estimate.quote(&pricing).unwrap(), expected);
        }
        let mut estimate = UsageFeatures::from_json(InferenceType::ImageGeneration, &json!({}))
            .unwrap()
            .estimate(&config, "test")
            .unwrap();
        estimate.policy.minimum_nano_usd = 1000;
        assert_eq!(estimate.quote(&pricing).unwrap(), 1000);
        let mut free = pricing.clone();
        free.cost_per_image = 0;
        assert_eq!(estimate.quote(&free).unwrap(), 0);
        let mut huge = pricing;
        huge.cost_per_image = i64::MAX;
        assert!(estimate.quote(&huge).is_err());
    }

    #[tokio::test]
    async fn provider_retry_gets_a_new_hold_and_does_not_reuse_an_unknown_attempt() {
        let execution = BudgetExecution::new(
            Arc::new(crate::test_utils::MockUsageService),
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
                inference_providers::retry_admission::before_retry()
                    .await
                    .unwrap();
                let second = execution.id().await.unwrap();
                assert_ne!(first, second);
            })
            .await;
    }
}
