use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::ports::UserId;
use crate::organization::OrganizationId;

// Domain ID types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WorkspaceId(pub Uuid);

impl From<Uuid> for WorkspaceId {
    fn from(uuid: Uuid) -> Self {
        WorkspaceId(uuid)
    }
}

impl std::fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyId(pub String);

// Domain models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub organization_id: OrganizationId,
    pub created_by_user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub is_active: bool,
    pub settings: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: ApiKeyId,
    /// Returned only on creation
    pub key: Option<String>,
    /// First 8-10 characters of the key for display purposes (e.g., "sk-abc123")
    pub key_prefix: String,
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub created_by_user_id: UserId,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub deleted_at: Option<DateTime<Utc>>,
    /// Optional spending limit in nano-dollars (scale 9, USD). None means no limit.
    pub spend_limit: Option<i64>,
    /// Total usage/spend in nano-dollars (scale 9, USD). None if not fetched.
    pub usage: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub workspace_id: WorkspaceId,
    pub created_by_user_id: UserId,
    pub expires_at: Option<DateTime<Utc>>,
    /// Optional spending limit in nano-dollars (scale 9, USD). None means no limit.
    pub spend_limit: Option<i64>,
}

// Error types
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("Workspace not found")]
    NotFound,

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

    #[error("Workspace already exists")]
    AlreadyExists,

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("API key not found")]
    ApiKeyNotFound,
}

// Repository trait for workspace data access
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    async fn get_by_id(&self, workspace_id: WorkspaceId) -> anyhow::Result<Option<Workspace>>;

    async fn get_workspace_with_organization(
        &self,
        workspace_id: WorkspaceId,
    ) -> anyhow::Result<Option<(Workspace, crate::organization::Organization)>>;

    async fn list_by_organization(
        &self,
        organization_id: OrganizationId,
    ) -> anyhow::Result<Vec<Workspace>>;

    /// List workspaces for an organization with pagination
    async fn list_by_organization_paginated(
        &self,
        organization_id: OrganizationId,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<Workspace>>;

    /// Create a new workspace
    async fn create(
        &self,
        name: String,
        display_name: String,
        description: Option<String>,
        organization_id: OrganizationId,
        created_by_user_id: UserId,
    ) -> anyhow::Result<Workspace>;

    /// Update a workspace
    async fn update(
        &self,
        workspace_id: WorkspaceId,
        display_name: Option<String>,
        description: Option<String>,
        settings: Option<serde_json::Value>,
    ) -> anyhow::Result<Option<Workspace>>;

    /// Delete (deactivate) a workspace
    async fn delete(&self, workspace_id: WorkspaceId) -> anyhow::Result<bool>;

    /// Count workspaces for an organization
    async fn count_by_organization(&self, organization_id: OrganizationId) -> anyhow::Result<i64>;
}

// Repository trait for API key data access
#[async_trait]
pub trait ApiKeyRepository: Send + Sync {
    async fn validate(&self, api_key: String) -> anyhow::Result<Option<ApiKey>>;

    async fn create(&self, request: CreateApiKeyRequest) -> anyhow::Result<ApiKey>;

    async fn get_by_id(&self, id: ApiKeyId) -> anyhow::Result<Option<ApiKey>>;

    async fn list_by_workspace_paginated(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<ApiKey>>;

    async fn delete(&self, id: ApiKeyId) -> anyhow::Result<bool>;

    async fn update_last_used(&self, id: ApiKeyId) -> anyhow::Result<()>;

    async fn update_spend_limit(
        &self,
        id: ApiKeyId,
        spend_limit: Option<i64>,
    ) -> anyhow::Result<ApiKey>;

    async fn update(
        &self,
        id: ApiKeyId,
        name: Option<String>,
        expires_at: Option<Option<DateTime<Utc>>>,
        spend_limit: Option<Option<i64>>,
        is_active: Option<bool>,
    ) -> anyhow::Result<ApiKey>;

    /// Count API keys for a workspace
    async fn count_by_workspace(&self, workspace_id: WorkspaceId) -> anyhow::Result<i64>;

    /// Check for duplicate API key name in workspace
    async fn check_name_duplication(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> anyhow::Result<bool>;

    /// Revoke (soft delete) an API key
    async fn revoke(&self, id: ApiKeyId) -> anyhow::Result<bool>;
}

// Service trait
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait WorkspaceServiceTrait: Send + Sync {
    /// Get a workspace by ID
    async fn get_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<Workspace, WorkspaceError>;

    /// Get a workspace with its organization
    async fn get_workspace_with_organization(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<(Workspace, crate::organization::Organization), WorkspaceError>;

    /// List workspaces for an organization
    async fn list_workspaces_for_organization(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<Vec<Workspace>, WorkspaceError>;

    /// List workspaces for an organization with pagination
    async fn list_workspaces_for_organization_paginated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Workspace>, WorkspaceError>;

    /// Create a new workspace in an organization with permission checking
    async fn create_workspace(
        &self,
        name: String,
        display_name: String,
        description: Option<String>,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<Workspace, WorkspaceError>;

    /// Create an API key for a workspace with permission checking
    async fn create_api_key(&self, request: CreateApiKeyRequest) -> Result<ApiKey, WorkspaceError>;

    /// List API keys for a workspace with pagination and permission checking
    async fn list_api_keys_paginated(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ApiKey>, WorkspaceError>;

    /// Get a specific API key by ID with permission checking
    async fn get_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
    ) -> Result<Option<ApiKey>, WorkspaceError>;

    /// Delete an API key with permission checking
    async fn delete_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError>;

    /// Update API key spend limit with permission checking
    async fn update_api_key_spend_limit(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
        spend_limit: Option<i64>,
    ) -> Result<ApiKey, WorkspaceError>;

    /// Update API key (name, expires_at, and/or spend_limit) with permission checking
    async fn update_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
        name: Option<String>,
        expires_at: Option<Option<DateTime<Utc>>>,
        spend_limit: Option<Option<i64>>,
        is_active: Option<bool>,
    ) -> Result<ApiKey, WorkspaceError>;

    /// Check if a user can manage API keys for a workspace
    async fn can_manage_api_keys(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<bool, WorkspaceError>;

    /// Update a workspace with permission checking
    async fn update_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
        display_name: Option<String>,
        description: Option<String>,
        settings: Option<serde_json::Value>,
    ) -> Result<Workspace, WorkspaceError>;

    /// Delete (deactivate) a workspace with permission checking
    async fn delete_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError>;

    /// Count workspaces for an organization with permission checking
    async fn count_workspaces_by_organization(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, WorkspaceError>;

    /// Count API keys for a workspace with permission checking
    async fn count_api_keys_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        requester_id: UserId,
    ) -> Result<i64, WorkspaceError>;

    /// Check for duplicate API key name in workspace
    async fn check_api_key_name_duplication(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError>;

    /// Revoke an API key (alias for delete_api_key with revoke semantics)
    async fn revoke_api_key(
        &self,
        workspace_id: WorkspaceId,
        api_key_id: ApiKeyId,
        requester_id: UserId,
    ) -> Result<bool, WorkspaceError>;
}
