pub mod analytics;
pub mod ports;

pub use analytics::{
    AnalyticsRepository, AnalyticsService, ApiKeyMetrics, MetricsSummary, ModelMetrics,
    OrganizationMetrics, PlatformMetrics, TimeSeriesMetrics, TimeSeriesPoint, TopModelMetrics,
    TopOrganizationMetrics, WorkspaceMetrics,
};
pub use ports::{PlatformServiceInfo, *};
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
        models: BatchUpdateModelAdminRequest,
    ) -> Result<BatchUpdateModelAdminResponse, AdminError> {
        if models.is_empty() {
            return Err(AdminError::InvalidPricing(
                "At least one model must be provided".to_string(),
            ));
        }

        // Validate all models first
        for (model_name, request) in &models {
            Self::validate_model_request(model_name, request, Arc::clone(&self.repository)).await?;
        }

        // Upsert all models
        let mut results = std::collections::HashMap::new();
        for (model_name, request) in models {
            let pricing = self
                .repository
                .upsert_model_pricing(&model_name, request)
                .await
                .map_err(|e| AdminError::InternalError(e.to_string()))?;
            results.insert(model_name, pricing);
        }

        Ok(results)
    }

    async fn get_model_history(
        &self,
        model_name: &str,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ModelHistoryEntry>, i64), AdminError> {
        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        let (history, total) = self
            .repository
            .get_model_history(model_name, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if total == 0 {
            return Err(AdminError::ModelNotFound(model_name.to_string()));
        }

        Ok((history, total))
    }

    async fn delete_model(
        &self,
        model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<(), AdminError> {
        // Validate model name
        if model_name.trim().is_empty() {
            return Err(AdminError::InvalidPricing(
                "Model name cannot be empty".to_string(),
            ));
        }

        let deleted = self
            .repository
            .soft_delete_model(
                model_name,
                change_reason,
                changed_by_user_id,
                changed_by_user_email,
            )
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if !deleted {
            return Err(AdminError::ModelNotFound(format!(
                "Model '{model_name}' not found"
            )));
        }

        Ok(())
    }

    async fn deprecate_model(
        &self,
        deprecated_model_name: &str,
        successor_model_name: &str,
        change_reason: Option<String>,
        changed_by_user_id: Option<uuid::Uuid>,
        changed_by_user_email: Option<String>,
    ) -> Result<DeprecateModelOutcome, AdminError> {
        let deprecated = deprecated_model_name.trim();
        let successor = successor_model_name.trim();

        if deprecated.is_empty() || successor.is_empty() {
            return Err(AdminError::InvalidDeprecation(
                "modelId and successorModelId are required".to_string(),
            ));
        }
        if deprecated == successor {
            return Err(AdminError::InvalidDeprecation(
                "modelId and successorModelId must differ".to_string(),
            ));
        }

        let outcome = self
            .repository
            .deprecate_model(
                deprecated,
                successor,
                change_reason,
                changed_by_user_id,
                changed_by_user_email,
            )
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| {
                AdminError::ModelNotFound(format!(
                    "Either '{deprecated}' or '{successor}' was not found, or the successor is not active"
                ))
            })?;

        Ok(outcome)
    }

    async fn update_organization_limits(
        &self,
        organization_id: uuid::Uuid,
        limits: OrganizationLimitsUpdate,
    ) -> Result<OrganizationLimits, AdminError> {
        // Validate limits
        Self::validate_organization_limits(&limits)?;

        let updated_limits = self
            .repository
            .update_organization_limits(organization_id, limits)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("Organization not found") {
                    AdminError::OrganizationNotFound(format!(
                        "Organization '{organization_id}' not found"
                    ))
                } else {
                    AdminError::InternalError(error_msg)
                }
            })?;

        Ok(updated_limits)
    }

    async fn get_organization_limits_history(
        &self,
        organization_id: uuid::Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<OrganizationLimitsHistoryEntry>, i64), AdminError> {
        let total = self
            .repository
            .count_organization_limits_history(organization_id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        let history = self
            .repository
            .get_organization_limits_history(organization_id, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        if history.is_empty() {
            return Err(AdminError::OrganizationNotFound(
                organization_id.to_string(),
            ));
        }

        Ok((history, total))
    }

    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<UserInfo>, i64), AdminError> {
        let users = self
            .repository
            .list_users(limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        let total = self
            .repository
            .get_active_user_count()
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((users, total))
    }

    async fn list_users_with_organizations(
        &self,
        limit: i64,
        offset: i64,
        search_by_name: Option<String>,
    ) -> Result<(Vec<(UserInfo, Option<UserOrganizationInfo>)>, i64), AdminError> {
        let (users_with_orgs, total) = self
            .repository
            .list_users_with_organizations(limit, offset, search_by_name)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((users_with_orgs, total))
    }

    async fn list_models(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminModelInfo>, i64), AdminError> {
        let (models, total) = self
            .repository
            .list_models(include_inactive, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((models, total))
    }

    async fn update_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
        concurrent_limit: Option<u32>,
    ) -> Result<(), AdminError> {
        // Validate limit if provided (u32 is already non-negative, just check for zero)
        if let Some(limit) = concurrent_limit {
            if limit == 0 {
                return Err(AdminError::InvalidLimits(
                    "Concurrent limit must be a positive integer".to_string(),
                ));
            }
        }

        self.repository
            .update_organization_concurrent_limit(organization_id, concurrent_limit)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("not found") || error_msg.contains("inactive") {
                    AdminError::OrganizationNotFound(format!(
                        "Organization '{}' not found",
                        organization_id
                    ))
                } else {
                    AdminError::InternalError(error_msg)
                }
            })
    }

    async fn get_organization_concurrent_limit(
        &self,
        organization_id: uuid::Uuid,
    ) -> Result<Option<u32>, AdminError> {
        self.repository
            .get_organization_concurrent_limit(organization_id)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("not found") || error_msg.contains("inactive") {
                    AdminError::OrganizationNotFound(format!(
                        "Organization '{}' not found",
                        organization_id
                    ))
                } else {
                    AdminError::InternalError(error_msg)
                }
            })
    }

    async fn list_organizations(
        &self,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<AdminOrganizationInfo>, i64), AdminError> {
        // Execute both queries in parallel for better performance
        let (organizations_result, total_result) = tokio::join!(
            self.repository.list_all_organizations(limit, offset),
            self.repository.count_all_organizations()
        );

        let organizations =
            organizations_result.map_err(|e| AdminError::InternalError(e.to_string()))?;
        let total = total_result.map_err(|e| AdminError::InternalError(e.to_string()))?;

        Ok((organizations, total))
    }

    async fn list_services(
        &self,
        include_inactive: bool,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<PlatformServiceInfo>, i64), AdminError> {
        self.repository
            .list_services(include_inactive, limit, offset)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))
    }

    async fn get_service_by_id(&self, id: uuid::Uuid) -> Result<PlatformServiceInfo, AdminError> {
        self.repository
            .get_service_by_id(id)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| AdminError::ServiceNotFound(format!("Service {id} not found")))
    }

    async fn create_service(
        &self,
        service_name: &str,
        display_name: &str,
        description: Option<&str>,
        unit: crate::service_usage::ports::ServiceUnit,
        cost_per_unit: i64,
    ) -> Result<PlatformServiceInfo, AdminError> {
        let name = service_name.trim();
        if name.is_empty() {
            return Err(AdminError::InvalidPricing(
                "Service name cannot be empty".to_string(),
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(AdminError::InvalidPricing(
                "Service name must contain only lowercase letters, digits, and underscores (e.g. web_search)".to_string(),
            ));
        }
        if cost_per_unit < 0 {
            return Err(AdminError::InvalidPricing(
                "Cost per unit cannot be negative".to_string(),
            ));
        }
        self.repository
            .create_service(name, display_name, description, unit, cost_per_unit)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))
    }

    async fn update_service(
        &self,
        id: uuid::Uuid,
        display_name: Option<&str>,
        description: Option<&str>,
        cost_per_unit: Option<i64>,
        is_active: Option<bool>,
    ) -> Result<PlatformServiceInfo, AdminError> {
        if let Some(c) = cost_per_unit {
            if c < 0 {
                return Err(AdminError::InvalidPricing(
                    "Cost per unit cannot be negative".to_string(),
                ));
            }
        }
        self.repository
            .update_service(id, display_name, description, cost_per_unit, is_active)
            .await
            .map_err(|e| AdminError::InternalError(e.to_string()))?
            .ok_or_else(|| AdminError::ServiceNotFound(format!("Service {id} not found")))
    }
}

impl AdminServiceImpl {
    async fn validate_model_request(
        model_name: &str,
        request: &UpdateModelAdminRequest,
        _repository: Arc<dyn AdminRepository>,
    ) -> Result<(), AdminError> {
        // All costs use fixed scale 9 (nano-dollars) and USD - no scale/currency validation needed

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

    fn validate_organization_limits(limits: &OrganizationLimitsUpdate) -> Result<(), AdminError> {
        // All amounts use fixed scale 9 (nano-dollars) and USD - no scale/currency validation needed

        // Validate amount is non-negative
        if limits.spend_limit < 0 {
            return Err(AdminError::InvalidLimits(
                "Spend limit cannot be negative".to_string(),
            ));
        }

        Ok(())
    }
}
