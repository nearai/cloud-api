use crate::repositories::ModelRepository;
use services::usage::ports::ModelPricing;

/// Trait implementation adapter for ModelRepository
#[async_trait::async_trait]
impl services::usage::ports::ModelRepository for ModelRepository {
    async fn get_model_by_name(&self, model_name: &str) -> anyhow::Result<Option<ModelPricing>> {
        let model = self.get_by_name(model_name).await?;

        Ok(model.map(|m| ModelPricing {
            input_cost_amount: m.input_cost_amount,
            input_cost_scale: m.input_cost_scale,
            input_cost_currency: m.input_cost_currency,
            output_cost_amount: m.output_cost_amount,
            output_cost_scale: m.output_cost_scale,
            output_cost_currency: m.output_cost_currency,
        }))
    }
}
