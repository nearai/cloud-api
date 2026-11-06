pub mod ports;

pub use ports::*;
use std::sync::Arc;

use crate::auth::ports::UserId;
use crate::common::RepositoryError;
use crate::organization::{MemberRole, OrganizationId, OrganizationServiceTrait};
use async_trait::async_trait;

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
                WorkspaceError::InternalError(format!("Resource not found: {msg}"))
            }
            RepositoryError::RequiredFieldMissing(field) => {
                WorkspaceError::InvalidParams(format!("Required field is missing: {field}"))
            }
            RepositoryError::ForeignKeyViolation(msg) => {
                WorkspaceError::InvalidParams(format!("Referenced entity does not exist: {msg}"))
            }
            RepositoryError::ValidationFailed(msg) => {
                WorkspaceError::InvalidParams(format!("Validation failed: {msg}"))
            }
            RepositoryError::DependencyExists(msg) => {
                WorkspaceError::InvalidParams(format!("Cannot delete due to dependencies: {msg}"))
            }
            RepositoryError::TransactionConflict => {
                WorkspaceError::InternalError("Transaction conflict, please retry".to_string())
            }
            RepositoryError::ConnectionFailed(msg) => {
                WorkspaceError::InternalError(format!("Database connection failed: {msg}"))
            }
            RepositoryError::AuthenticationFailed => {
                WorkspaceError::InternalError("Database authentication failed".to_string())
            }
            RepositoryError::QueryTimeout => {
                WorkspaceError::InternalError("Database query timed out".to_string())
            }
            RepositoryError::PoolError(err) => {
                WorkspaceError::InternalError(format!("Database connection pool error: {err}"))
            }
            RepositoryError::DatabaseError(err) => {
                WorkspaceError::InternalError(format!("Database operation failed: {err}"))
            }
            RepositoryError::DataConversionError(err) => {
                WorkspaceError::InternalError(format!("Data conversion error: {err}"))
            }
        }
    }

    /// Helper: Check if user has permission to manage workspace resources
    async fn check_workspace_permission(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<(Workspace, crate::organization::Organization), WorkspaceError> {
        let (workspace, organization, _) =
            self.workspace_member_role(workspace_id, user_id).await?;
        Ok((workspace, organization))
    }

    /// The workspace, its organization and the requester's role in that organization.
    /// `Unauthorized` when the requester is not a member.
    async fn workspace_member_role(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<(Workspace, crate::organization::Organization, MemberRole), WorkspaceError> {
        // Get workspace with organization
        let (workspace, organization) = self
            .workspace_repository
            .get_workspace_with_organization(workspace_id)
            .await
            .map_err(Self::map_repository_error)?
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

        let Some(role) = member else {
            return Err(WorkspaceError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        };

        Ok((workspace, organization, role))
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
            .map_err(Self::map_repository_error)
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
            .map_err(Self::map_repository_error)
    }

    async fn list_api_keys_paginated(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<ApiKeyOrderBy>,
        order_direction: Option<ApiKeyOrderDirection>,
    ) -> Result<Vec<ApiKey>, WorkspaceError> {
        // Check permissions
        self.check_workspace_permission(workspace_id.clone(), requester_id)
            .await?;

        // List API keys with pagination (repository now includes usage data via JOIN)
        self.api_key_repository
            .list_by_workspace_paginated(workspace_id, limit, offset, order_by, order_direction)
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

    async fn list_workspaces_for_user(
        &self,
        user_id: UserId,
        limit: i64,
    ) -> Result<Vec<Workspace>, WorkspaceError> {
        self.workspace_repository
            .list_by_user(user_id, limit)
            .await
            .map_err(Self::map_repository_error)
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
            .map_err(Self::map_repository_error)
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
}
