use crate::models::{UpdateModelPricingRequest, UpdateOrganizationLimitsDbRequest};
use crate::pool::DbPool;
use crate::repositories::{
    ModelAliasRepository, ModelRepository, OrganizationLimitsRepository, UserRepository,
};
use anyhow::Result;
use async_trait::async_trait;
use services::admin::{
    AdminRepository, ModelPricing, ModelPricingHistoryEntry, OrganizationLimits,
    OrganizationLimitsHistoryEntry, OrganizationLimitsUpdate, UpdateModelAdminRequest, UserInfo,
};
use std::sync::Arc;
use uuid::Uuid;

/// Composite repository that implements AdminRepository for both model and organization operations
#[derive(Clone)]
pub struct AdminCompositeRepository {
    model_repo: Arc<ModelRepository>,
    alias_repo: Arc<ModelAliasRepository>,
    limits_repo: Arc<OrganizationLimitsRepository>,
    user_repo: Arc<UserRepository>,
}

impl AdminCompositeRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            model_repo: Arc::new(ModelRepository::new(pool.clone())),
            alias_repo: Arc::new(ModelAliasRepository::new(pool.clone())),
            limits_repo: Arc::new(OrganizationLimitsRepository::new(pool.clone())),
            user_repo: Arc::new(UserRepository::new(pool)),
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
            public_name: request.public_name,
            input_cost_per_token: request.input_cost_per_token,
            output_cost_per_token: request.output_cost_per_token,
            model_display_name: request.model_display_name,
            model_description: request.model_description,
            model_icon: request.model_icon,
            context_length: request.context_length,
            verifiable: request.verifiable,
            is_active: request.is_active,
            aliases: request.aliases.clone(),
        };

        let model = self
            .model_repo
            .upsert_model_pricing(model_name, &db_request)
            .await?;

        // Handle aliases if provided
        if let Some(alias_names) = request.aliases {
            self.alias_repo
                .upsert_aliases_for_model(&model.id, &alias_names)
                .await?;
        }

        Ok(ModelPricing {
            public_name: model.public_name,
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

    async fn get_pricing_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelPricingHistoryEntry>, i64)> {
        let total = self
            .model_repo
            .count_pricing_history_by_name(model_name)
            .await?;

        let history = self
            .model_repo
            .get_pricing_history_by_name(model_name, limit, offset)
            .await?;

        let entries = history
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
            .collect();

        Ok((entries, total))
    }

    async fn soft_delete_model(&self, model_name: &str) -> Result<bool> {
        self.model_repo.soft_delete_model(model_name).await
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

    async fn count_organization_limits_history(&self, organization_id: Uuid) -> Result<i64> {
        self.limits_repo.count_limits_history(organization_id).await
    }

    async fn get_organization_limits_history(
        &self,
        organization_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationLimitsHistoryEntry>> {
        let history = self
            .limits_repo
            .get_limits_history(organization_id, limit, offset)
            .await?;

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

    async fn list_users(&self, limit: i64, offset: i64) -> Result<Vec<UserInfo>> {
        let users = self.user_repo.list(limit, offset).await?;

        Ok(users
            .into_iter()
            .map(|u| UserInfo {
                id: u.id,
                email: u.email,
                username: u.username,
                display_name: u.display_name,
                avatar_url: u.avatar_url,
                created_at: u.created_at,
                last_login_at: u.last_login_at,
                is_active: u.is_active,
            })
            .collect())
    }
}
