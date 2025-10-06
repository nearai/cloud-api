use crate::models::{UpdateModelPricingRequest, UpdateOrganizationLimitsDbRequest};
use crate::pool::DbPool;
use crate::repositories::{ModelRepository, OrganizationLimitsRepository};
use anyhow::Result;
use async_trait::async_trait;
use services::admin::{
    AdminRepository, ModelPricing, ModelPricingHistoryEntry, OrganizationLimits,
    OrganizationLimitsHistoryEntry, OrganizationLimitsUpdate, UpdateModelAdminRequest,
};
use std::sync::Arc;
use uuid::Uuid;

/// Composite repository that implements AdminRepository for both model and organization operations
#[derive(Clone)]
pub struct AdminCompositeRepository {
    model_repo: Arc<ModelRepository>,
    limits_repo: Arc<OrganizationLimitsRepository>,
}

impl AdminCompositeRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            model_repo: Arc::new(ModelRepository::new(pool.clone())),
            limits_repo: Arc::new(OrganizationLimitsRepository::new(pool)),
        }
    }
}

#[async_trait]
impl AdminRepository for AdminCompositeRepository {
    async fn upsert_model_pricing(
        &self,
        model_name: &str,
        request: UpdateModelAdminRequest,
    ) -> Result<ModelPricing> {
        // Convert service request to database request
        let db_request = UpdateModelPricingRequest {
            input_cost_per_token: request.input_cost_per_token,
            output_cost_per_token: request.output_cost_per_token,
            model_display_name: request.model_display_name,
            model_description: request.model_description,
            model_icon: request.model_icon,
            context_length: request.context_length,
            verifiable: request.verifiable,
            is_active: request.is_active,
        };

        let model = self
            .model_repo
            .upsert_model_pricing(model_name, &db_request)
            .await?;

        Ok(ModelPricing {
            model_display_name: model.model_display_name,
            model_description: model.model_description,
            model_icon: model.model_icon,
            input_cost_per_token: model.input_cost_per_token,
            output_cost_per_token: model.output_cost_per_token,
            context_length: model.context_length,
            verifiable: model.verifiable,
            is_active: model.is_active,
        })
    }

    async fn get_pricing_history(&self, model_name: &str) -> Result<Vec<ModelPricingHistoryEntry>> {
        let history = self
            .model_repo
            .get_pricing_history_by_name(model_name)
            .await?;

        Ok(history
            .into_iter()
            .map(|h| ModelPricingHistoryEntry {
                id: h.id,
                model_id: h.model_id,
                input_cost_per_token: h.input_cost_per_token,
                output_cost_per_token: h.output_cost_per_token,
                context_length: h.context_length,
                model_display_name: h.model_display_name,
                model_description: h.model_description,
                effective_from: h.effective_from,
                effective_until: h.effective_until,
                changed_by: h.changed_by,
                change_reason: h.change_reason,
                created_at: h.created_at,
            })
            .collect())
    }

    async fn update_organization_limits(
        &self,
        organization_id: Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits> {
        let db_request = UpdateOrganizationLimitsDbRequest {
            spend_limit: limits.spend_limit,
            changed_by: limits.changed_by,
            change_reason: limits.change_reason,
        };

        let history = self
            .limits_repo
            .update_limits(organization_id, &db_request)
            .await?;

        Ok(OrganizationLimits {
            organization_id: history.organization_id,
            spend_limit: history.spend_limit,
            effective_from: history.effective_from,
        })
    }

    async fn get_current_organization_limits(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<OrganizationLimits>> {
        let limits_opt = self.limits_repo.get_current_limits(organization_id).await?;

        Ok(limits_opt.map(|h| OrganizationLimits {
            organization_id: h.organization_id,
            spend_limit: h.spend_limit,
            effective_from: h.effective_from,
        }))
    }

    async fn get_organization_limits_history(
        &self,
        organization_id: Uuid,
    ) -> Result<Vec<OrganizationLimitsHistoryEntry>> {
        let history = self.limits_repo.get_limits_history(organization_id).await?;

        Ok(history
            .into_iter()
            .map(|h| OrganizationLimitsHistoryEntry {
                id: h.id,
                organization_id: h.organization_id,
                spend_limit: h.spend_limit,
                effective_from: h.effective_from,
                effective_until: h.effective_until,
                changed_by: h.changed_by,
                change_reason: h.change_reason,
                created_at: h.created_at,
            })
            .collect())
    }
}
