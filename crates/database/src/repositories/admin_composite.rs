use crate::models::{UpdateModelPricingRequest, UpdateOrganizationLimitsDbRequest};
use crate::pool::DbPool;
use crate::repositories::{
    ModelAliasRepository, ModelRepository, OrganizationLimitsRepository, UserRepository,
};
use anyhow::Result;
use async_trait::async_trait;
use services::admin::{
    AdminModelInfo, AdminRepository, ModelHistoryEntry, ModelPricing, OrganizationLimits,
    OrganizationLimitsHistoryEntry, OrganizationLimitsUpdate, UpdateModelAdminRequest, UserInfo,
    UserOrganizationInfo,
};
use std::sync::Arc;
use uuid::Uuid;

/// Composite repository that implements AdminRepository for both model and organization operations
#[derive(Clone)]
pub struct AdminCompositeRepository {
    pool: DbPool,
    model_repo: Arc<ModelRepository>,
    alias_repo: Arc<ModelAliasRepository>,
    limits_repo: Arc<OrganizationLimitsRepository>,
    user_repo: Arc<UserRepository>,
}

impl AdminCompositeRepository {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool: pool.clone(),
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
            input_cost_per_token: request.input_cost_per_token,
            output_cost_per_token: request.output_cost_per_token,
            model_display_name: request.model_display_name,
            model_description: request.model_description,
            model_icon: request.model_icon,
            context_length: request.context_length,
            verifiable: request.verifiable,
            is_active: request.is_active,
            aliases: request.aliases.clone(),
            owned_by: request.owned_by,
            change_reason: request.change_reason,
            changed_by_user_id: request.changed_by_user_id,
            changed_by_user_email: request.changed_by_user_email,
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
            model_display_name: model.model_display_name,
            model_description: model.model_description,
            model_icon: model.model_icon,
            input_cost_per_token: model.input_cost_per_token,
            output_cost_per_token: model.output_cost_per_token,
            context_length: model.context_length,
            verifiable: model.verifiable,
            is_active: model.is_active,
            aliases: model.aliases,
            owned_by: model.owned_by,
        })
    }

    async fn get_model_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHistoryEntry>, i64)> {
        let total = self
            .model_repo
            .count_model_history_by_name(model_name)
            .await?;

        let history = self
            .model_repo
            .get_model_history_by_name(model_name, limit, offset)
            .await?;

        let entries = history
            .into_iter()
            .map(|h| ModelHistoryEntry {
                id: h.id,
                model_id: h.model_id,
                input_cost_per_token: h.input_cost_per_token,
                output_cost_per_token: h.output_cost_per_token,
                context_length: h.context_length,
                model_name: h.model_name,
                model_display_name: h.model_display_name,
                model_description: h.model_description,
                model_icon: h.model_icon,
                verifiable: h.verifiable,
                is_active: h.is_active,
                owned_by: h.owned_by,
                effective_from: h.effective_from,
                effective_until: h.effective_until,
                changed_by_user_id: h.changed_by_user_id,
                changed_by_user_email: h.changed_by_user_email,
                change_reason: h.change_reason,
                created_at: h.created_at,
            })
            .collect();

        Ok((entries, total))
    }

    async fn soft_delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<bool> {
        self.model_repo
            .soft_delete_model(
                model_name,
                change_reason,
                changed_by_user_id,
                changed_by_user_email,
            )
            .await
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
            changed_by_user_id: limits.changed_by_user_id,
            changed_by_user_email: limits.changed_by_user_email,
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
                changed_by_user_id: h.changed_by_user_id,
                changed_by_user_email: h.changed_by_user_email,
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

    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64)> {
        let (users_with_orgs, total_count) = self
            .user_repo
            .list_with_organizations(limit, offset, search_by_name)
            .await?;

        let result = users_with_orgs
            .into_iter()
            .map(|(u, org_data)| {
                let user_info = UserInfo {
                    id: u.id,
                    email: u.email,
                    username: u.username,
                    display_name: u.display_name,
                    avatar_url: u.avatar_url,
                    created_at: u.created_at,
                    last_login_at: u.last_login_at,
                    is_active: u.is_active,
                };
                (user_info, org_data)
            })
            .collect();

        Ok((result, total_count))
    }

    async fn get_active_user_count(&self) -> Result<i64> {
        self.user_repo.get_active_user_count().await
    }

    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64)> {
        let total = self
            .model_repo
            .get_all_models_count(include_inactive)
            .await?;

        let models = self
            .model_repo
            .get_all_models(include_inactive, limit, offset)
            .await?;

        let admin_models = models
            .into_iter()
            .map(|m| AdminModelInfo {
                id: m.id,
                model_name: m.model_name,
                model_display_name: m.model_display_name,
                model_description: m.model_description,
                model_icon: m.model_icon,
                input_cost_per_token: m.input_cost_per_token,
                output_cost_per_token: m.output_cost_per_token,
                context_length: m.context_length,
                verifiable: m.verifiable,
                is_active: m.is_active,
                owned_by: m.owned_by,
                aliases: m.aliases,
                created_at: m.created_at,
                updated_at: m.updated_at,
            })
            .collect();

        Ok((admin_models, total))
    }

    async fn update_organization_concurrent_limit(
        &self,
        organization_id: Uuid,
        concurrent_limit: Option<u32>,
    ) -> Result<()> {
        let client = self.pool.get().await?;

        // Convert u32 to i32 for PostgreSQL INTEGER type
        let db_limit: Option<i32> = concurrent_limit.map(|v| v as i32);

        let rows_updated = client
            .execute(
                "UPDATE organizations SET rate_limit = $1, updated_at = NOW() WHERE id = $2 AND is_active = true",
                &[&db_limit, &organization_id],
            )
            .await?;

        if rows_updated == 0 {
            anyhow::bail!("Organization not found or inactive: {}", organization_id);
        }

        Ok(())
    }

    async fn get_organization_concurrent_limit(
        &self,
        organization_id: Uuid,
    ) -> Result<Option<u32>> {
        let client = self.pool.get().await?;

        let row = client
            .query_opt(
                "SELECT rate_limit FROM organizations WHERE id = $1 AND is_active = true",
                &[&organization_id],
            )
            .await?;

        match row {
            Some(r) => {
                let db_limit: Option<i32> = r.get("rate_limit");
                // Convert i32 from DB to u32, filtering out non-positive values
                Ok(db_limit.and_then(|v| u32::try_from(v).ok()))
            }
            None => anyhow::bail!("Organization not found or inactive: {}", organization_id),
        }
    }
}
