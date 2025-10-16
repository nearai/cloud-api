use crate::repositories::ModelRepository;
use services::usage::ports::ModelPricing;

/// Trait implementation adapter for ModelRepository
#[async_trait::async_trait]
impl services::usage::ports::ModelRepository for ModelRepository {
    async fn get_model_by_name(&self, model_name: &str) -> anyhow::Result<Option<ModelPricing>> {
        let model = self.get_active_model_by_name(model_name).await?;

        Ok(model.map(|m| ModelPricing {
            input_cost_per_token: m.input_cost_per_token,
            output_cost_per_token: m.output_cost_per_token,
        }))
    }
}
