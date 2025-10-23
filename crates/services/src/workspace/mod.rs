pub mod ports;

pub use ports::*;
use std::sync::Arc;

use crate::auth::ports::UserId;
use crate::common::RepositoryError;
use crate::organization::{OrganizationId, OrganizationServiceTrait};
use async_trait::async_trait;
use chrono::{DateTime, Utc};

pub struct WorkspaceServiceImpl {
    workspace_repository: Arc<dyn WorkspaceRepository>,
    api_key_repository: Arc<dyn ApiKeyRepository>,
    organization_service: Arc<dyn OrganizationServiceTrait>,
}

impl WorkspaceServiceImpl {
    pub fn new(
        workspace_repository: Arc<dyn WorkspaceRepository>,
        api_key_repository: Arc<dyn ApiKeyRepository>,
        organization_service: Arc<dyn OrganizationServiceTrait>,
    ) -> Self {
        Self {
            workspace_repository,
            api_key_repository,
            organization_service,
        }
    }

    /// Convert RepositoryError to WorkspaceError
    fn map_repository_error(err: RepositoryError) -> WorkspaceError {
        match err {
            RepositoryError::AlreadyExists => WorkspaceError::AlreadyExists,
            RepositoryError::NotFound(msg) => {
                WorkspaceError::InternalError(format!("Resource not found: {}", msg))
            }
            RepositoryError::RequiredFieldMissing(field) => {
                WorkspaceError::InvalidParams(format!("Required field is missing: {}", field))
            }
            RepositoryError::ForeignKeyViolation(msg) => {
                WorkspaceError::InvalidParams(format!("Referenced entity does not exist: {}", msg))
            }
            RepositoryError::ValidationFailed(msg) => {
                WorkspaceError::InvalidParams(format!("Validation failed: {}", msg))
            }
            RepositoryError::DependencyExists(msg) => {
                WorkspaceError::InvalidParams(format!("Cannot delete due to dependencies: {}", msg))
            }
            RepositoryError::TransactionConflict => {
                WorkspaceError::InternalError("Transaction conflict, please retry".to_string())
            }
            RepositoryError::ConnectionFailed(msg) => {
                WorkspaceError::InternalError(format!("Database connection failed: {}", msg))
            }
            RepositoryError::AuthenticationFailed => {
                WorkspaceError::InternalError("Database authentication failed".to_string())
            }
            RepositoryError::PoolError(err) => {
                WorkspaceError::InternalError(format!("Database connection pool error: {}", err))
            }
            RepositoryError::DatabaseError(err) => {
                WorkspaceError::InternalError(format!("Database operation failed: {}", err))
            }
            RepositoryError::DataConversionError(err) => {
                WorkspaceError::InternalError(format!("Data conversion error: {}", err))
            }
        }
    }

    /// Helper: Check if user has permission to manage workspace resources
    async fn check_workspace_permission(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<(Workspace, crate::organization::Organization), WorkspaceError> {
        // Get workspace with organization
        let (workspace, organization) = self
            .workspace_repository
            .get_workspace_with_organization(workspace_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get workspace: {e}")))?
            .ok_or(WorkspaceError::NotFound)?;

        // Check if user is a member of the organization
        let member = self
            .organization_service
            .get_user_role(organization.id.clone(), user_id)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to check organization membership: {e}"
                ))
            })?;

        if member.is_none() {
            return Err(WorkspaceError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        Ok((workspace, organization))
    }
}

#[async_trait]
impl WorkspaceServiceTrait for WorkspaceServiceImpl {
    async fn get_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<Workspace, WorkspaceError> {
        let (workspace, _) = self
            .check_workspace_permission(workspace_id, requester_id)
            .await?;
        Ok(workspace)
    }

    async fn get_workspace_by_name(
        &self,
        organization_id: OrganizationId,
        workspace_name: &str,
    ) -> Result<Option<Workspace>, WorkspaceError> {
        self.workspace_repository
            .get_by_name(organization_id.0, workspace_name)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get workspace: {e}")))
    }

    async fn get_workspace_with_organization(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<(Workspace, crate::organization::Organization), WorkspaceError> {
        self.check_workspace_permission(workspace_id, requester_id)
            .await
    }

    async fn list_workspaces_for_organization(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<Vec<Workspace>, WorkspaceError> {
        // Check if user is a member of the organization
        let is_member = self
            .organization_service
            .is_member(organization_id.clone(), requester_id)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to check organization membership: {e}"
                ))
            })?;

        if !is_member {
            return Err(WorkspaceError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        // List workspaces
        self.workspace_repository
            .list_by_organization(organization_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to list workspaces: {e}")))
    }

    async fn list_workspaces_for_organization_paginated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<WorkspaceOrderBy>,
        order_direction: Option<WorkspaceOrderDirection>,
    ) -> Result<Vec<Workspace>, WorkspaceError> {
        // Check if user is a member of the organization
        let is_member = self
            .organization_service
            .is_member(organization_id.clone(), requester_id)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to check organization membership: {e}"
                ))
            })?;

        if !is_member {
            return Err(WorkspaceError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        // List workspaces with pagination
        self.workspace_repository
            .list_by_organization_paginated(
                organization_id,
                limit,
                offset,
                order_by,
                order_direction,
            )
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to list workspaces with pagination: {e}"
                ))
            })
    }

    async fn create_workspace(
        &self,
        name: String,
        display_name: String,
        description: Option<String>,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<Workspace, WorkspaceError> {
        // Check if user is a member of the organization
        let is_member = self
            .organization_service
            .is_member(organization_id.clone(), requester_id.clone())
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to check organization membership: {e}"
                ))
            })?;

        if !is_member {
            return Err(WorkspaceError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        // Create the workspace
        self.workspace_repository
            .create(
                name,
                display_name,
                description,
                organization_id,
                requester_id,
            )
            .await
            .map_err(Self::map_repository_error)
    }

    async fn create_api_key(&self, request: CreateApiKeyRequest) -> Result<ApiKey, WorkspaceError> {
        let workspace_id = request.workspace_id.clone();
        let requester_id = request.created_by_user_id.clone();

        // Check permissions
        let (workspace, _) = self
            .check_workspace_permission(workspace_id, requester_id)
            .await?;

        // Verify the request matches the workspace
        if workspace.id.0 != request.workspace_id.0 {
            return Err(WorkspaceError::InvalidParams(
                "Workspace ID mismatch".to_string(),
            ));
        }

        // Create the API key
        self.api_key_repository
            .create(request)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to create API key: {e}")))
    }

    async fn list_api_keys_paginated(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ApiKey>, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // List API keys with pagination (repository now includes usage data via JOIN)
        self.api_key_repository
            .list_by_workspace_paginated(workspace_id, limit, offset)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to list API keys with pagination: {e}"
                ))
            })
    }

    async fn get_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
    ) -> Result<Option<ApiKey>, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Get the API key
        let api_key = self
            .api_key_repository
            .get_by_id(api_key_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get API key: {e}")))?;

        // Verify it belongs to this workspace
        if let Some(ref key) = api_key {
            if key.workspace_id.0 != workspace_id.0 {
                return Ok(None);
            }
        }

        Ok(api_key)
    }

    async fn delete_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Verify the API key belongs to this workspace
        let api_key = self
            .api_key_repository
            .get_by_id(api_key_id.clone())
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get API key: {e}")))?
            .ok_or(WorkspaceError::ApiKeyNotFound)?;

        if api_key.workspace_id.0 != workspace_id.0 {
            return Err(WorkspaceError::ApiKeyNotFound);
        }

        // Delete the API key
        self.api_key_repository
            .delete(api_key_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to delete API key: {e}")))
    }

    async fn update_api_key_spend_limit(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
        spend_limit: Option<i64>,
    ) -> Result<ApiKey, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Verify the API key belongs to this workspace
        let api_key = self
            .api_key_repository
            .get_by_id(api_key_id.clone())
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get API key: {e}")))?
            .ok_or(WorkspaceError::ApiKeyNotFound)?;

        if api_key.workspace_id.0 != workspace_id.0 {
            return Err(WorkspaceError::ApiKeyNotFound);
        }

        // Update the spend limit
        self.api_key_repository
            .update_spend_limit(api_key_id, spend_limit)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!("Failed to update API key spend limit: {e}"))
            })
    }

    async fn update_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
        name: Option<String>,
        expires_at: Option<Option<DateTime<Utc>>>,
        spend_limit: Option<Option<i64>>,
        is_active: Option<bool>,
    ) -> Result<ApiKey, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Verify the API key belongs to this workspace
        let api_key = self
            .api_key_repository
            .get_by_id(api_key_id.clone())
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get API key: {e}")))?
            .ok_or(WorkspaceError::ApiKeyNotFound)?;

        if api_key.workspace_id.0 != workspace_id.0 {
            return Err(WorkspaceError::ApiKeyNotFound);
        }

        // Update the API key
        self.api_key_repository
            .update(api_key_id, name, expires_at, spend_limit, is_active)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to update API key: {e}")))
    }

    async fn can_manage_api_keys(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<bool, WorkspaceError> {
        // Try to check permissions - if it succeeds, user can manage
        match self.check_workspace_permission(workspace_id, user_id).await {
            Ok(_) => Ok(true),
            Err(WorkspaceError::Unauthorized(_)) => Ok(false),
            Err(WorkspaceError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn update_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
        display_name: Option<String>,
        description: Option<String>,
        settings: Option<serde_json::Value>,
    ) -> Result<Workspace, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Update the workspace
        self.workspace_repository
            .update(workspace_id, display_name, description, settings)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to update workspace: {e}")))?
            .ok_or(WorkspaceError::NotFound)
    }

    async fn delete_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Delete the workspace
        self.workspace_repository
            .delete(workspace_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to delete workspace: {e}")))
    }

    async fn count_workspaces_by_organization(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, WorkspaceError> {
        // Check if user is a member of the organization
        let is_member = self
            .organization_service
            .is_member(organization_id.clone(), requester_id)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to check organization membership: {e}"
                ))
            })?;

        if !is_member {
            return Err(WorkspaceError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        // Count workspaces
        self.workspace_repository
            .count_by_organization(organization_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to count workspaces: {e}")))
    }

    async fn count_api_keys_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<i64, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Count API keys
        self.api_key_repository
            .count_by_workspace(workspace_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to count API keys: {e}")))
    }

    async fn check_api_key_name_duplication(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Check for duplication
        self.api_key_repository
            .check_name_duplication(workspace_id, name)
            .await
            .map_err(|e| {
                WorkspaceError::InternalError(format!(
                    "Failed to check API key name duplication: {e}"
                ))
            })
    }

    async fn revoke_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // Verify the API key belongs to this workspace
        let api_key = self
            .api_key_repository
            .get_by_id(api_key_id.clone())
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to get API key: {e}")))?
            .ok_or(WorkspaceError::ApiKeyNotFound)?;

        if api_key.workspace_id.0 != workspace_id.0 {
            return Err(WorkspaceError::ApiKeyNotFound);
        }

        // Revoke the API key
        self.api_key_repository
            .revoke(api_key_id)
            .await
            .map_err(|e| WorkspaceError::InternalError(format!("Failed to revoke API key: {e}")))
    }
}
