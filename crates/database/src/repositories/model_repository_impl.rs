use crate::repositories::ModelRepository;
use services::usage::ports::ModelPricing;
use uuid::Uuid;

/// Trait implementation adapter for ModelRepository
#[async_trait::async_trait]
impl services::usage::ports::ModelRepository for ModelRepository {
    async fn get_model_by_name(&self, model_name: &str) -> anyhow::Result<Option<ModelPricing>> {
        let model = self.get_active_model_by_name(model_name).await?;

        model.map(model_pricing_from_db).transpose()
    }

    async fn get_model_by_id(&self, model_id: Uuid) -> anyhow::Result<Option<ModelPricing>> {
        let model = self.get_by_id(&model_id).await?;

        model.map(model_pricing_from_db).transpose()
    }
}

fn model_pricing_from_db(m: crate::models::Model) -> anyhow::Result<ModelPricing> {
    let text_pricing = m
        .text_pricing
        .map(services::usage::TextPricingProfile::from_json)
        .transpose()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(ModelPricing {
        id: m.id,
        model_name: m.model_name,
        input_cost_per_token: m.input_cost_per_token,
        output_cost_per_token: m.output_cost_per_token,
        cost_per_image: m.cost_per_image,
        cache_read_cost_per_token: m.cache_read_cost_per_token,
        text_pricing,
    })
}
