pub mod ports;

pub use ports::*;
use std::sync::Arc;

pub struct AdminServiceImpl {
    repository: Arc<dyn AdminRepository>,
}

impl AdminServiceImpl {
    pub fn new(repository: Arc<dyn AdminRepository>) -> Self {
        Self { repository }
    }
}

#[async_trait::async_trait]
impl AdminService for AdminServiceImpl {
    async fn batch_upsert_models(
        &self,
        models: Vec<(String, UpdateModelAdminRequest)>,
    ) -> Result<Vec<ModelPricing>, AdminError> {
        if models.is_empty() {
            return Err(AdminError::InvalidPricing(
                "At least one model must be provided".to_string(),
            ));
        }

        // Validate all models first
        for (model_name, request) in &models {
            Self::validate_model_request(model_name, request)?;
        }

        // Upsert all models
        let mut results = Vec::new();
        for (model_name, request) in models {
            let pricing = self
                .repository
                .upsert_model_pricing(&model_name, request)
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?;
            results.push(pricing);
        }

        Ok(results)
    }

    async fn get_pricing_history(
        &self,
        model_name: &str,
    ) -> Result<Vec<ModelPricingHistoryEntry>, AdminError> {
        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        let history = self
            .repository
            .get_pricing_history(model_name)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if history.is_empty() {
            return Err(AdminError::ModelNotFound(model_name.to_string()));
        }

        Ok(history)
    }
}

impl AdminServiceImpl {
    fn validate_model_request(
        model_name: &str,
        request: &UpdateModelAdminRequest,
    ) -> Result<(), AdminError> {
        // Validate pricing scales if provided
        if let Some(scale) = request.input_cost_scale {
            if !(0..=20).contains(&scale) {
                return Err(AdminError::InvalidPricing(
                    "Input cost scale must be between 0 and 20".to_string(),
                ));
            }
        }

        if let Some(scale) = request.output_cost_scale {
            if !(0..=20).contains(&scale) {
                return Err(AdminError::InvalidPricing(
                    "Output cost scale must be between 0 and 20".to_string(),
                ));
            }
        }

        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        // Validate display name if provided
        if let Some(ref display_name) = request.model_display_name {
            if display_name.trim().is_empty() {
                return Err(AdminError::InvalidPricing(
                    "Model display name cannot be empty".to_string(),
                ));
            }
        }

        // Validate description if provided
        if let Some(ref description) = request.model_description {
            if description.trim().is_empty() {
                return Err(AdminError::InvalidPricing(
                    "Model description cannot be empty".to_string(),
                ));
            }
        }

        Ok(())
    }
}
