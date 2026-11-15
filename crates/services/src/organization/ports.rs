use super::super::auth::ports::{User, UserId};
use super::super::common::RepositoryError;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct OrganizationId(pub Uuid);

impl From<Uuid> for OrganizationId {
    fn from(uuid: Uuid) -> Self {
        OrganizationId(uuid)
    }
}

impl std::fmt::Display for OrganizationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Organization {
    /// Operator-controlled scheduler priority. Never accepted from or exposed in JSON.
    #[serde(skip)]
    pub request_priority: inference_providers::models::RequestPriority,
    pub id: OrganizationId,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: UserId,
    pub settings: serde_json::Value,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Organization {
    /// Effective organization fallback policy. Missing or malformed legacy
    /// values preserve the platform's existing fallback-enabled behavior.
    pub fn fallback_enabled(&self) -> bool {
        self.settings
            .get("fallback_enabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
    }

    /// The models this organization's owners and admins allow it to use
    /// (`allowed_models`, canonical names). `None`: every model.
    ///
    /// The validated write path is `PATCH /v1/organizations/{org_id}/settings`.
    /// A stored value that is not a list of strings, which only a full
    /// settings `PUT` can write, allows no model: the filter fails closed.
    pub fn allowed_models(&self) -> Option<std::collections::HashSet<String>> {
        match self.settings.get(ALLOWED_MODELS_SETTING) {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::Array(entries)) => {
                let names: Option<std::collections::HashSet<String>> = entries
                    .iter()
                    .map(|entry| entry.as_str().map(str::to_string))
                    .collect();
                Some(names.unwrap_or_else(|| {
                    tracing::warn!(
                        organization_id = %self.id,
                        "allowed_models setting has entries that are not model names; allowing no model"
                    );
                    std::collections::HashSet::new()
                }))
            }
            Some(_) => {
                tracing::warn!(
                    organization_id = %self.id,
                    "allowed_models setting is not a list; allowing no model"
                );
                Some(std::collections::HashSet::new())
            }
        }
    }
}

/// The organization setting that lists the models it may use.
pub const ALLOWED_MODELS_SETTING: &str = "allowed_models";

#[derive(Debug, Clone, PartialEq)]
pub struct OrganizationSettings {
    pub system_prompt: Option<String>,
    pub fallback_enabled: bool,
    /// Canonical names, sorted. `None`: every model.
    pub allowed_models: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct PatchOrganizationSettings {
    /// None = omitted, Some(None) = clear, Some(Some(value)) = set.
    pub system_prompt: Option<Option<String>>,
    /// None = omitted, Some(None) = allow every model, Some(Some(names)) = allow
    /// only these canonical names (validated by the caller, never empty).
    pub allowed_models: Option<Option<Vec<String>>>,
    /// None = omitted, Some(None) = restore the default, Some(Some(value)) = set.
    pub fallback_enabled: Option<Option<bool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationWithRole {
    pub organization: Organization,
    pub role: MemberRole,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationMember {
    pub organization_id: OrganizationId,
    pub user_id: UserId,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
}

impl MemberRole {
    pub fn can_manage_organization(&self) -> bool {
        matches!(self, MemberRole::Owner | MemberRole::Admin)
    }

    pub fn can_manage_api_keys(&self) -> bool {
        // All members can create and manage their own API keys
        true
    }

    pub fn can_delete_organization(&self) -> bool {
        matches!(self, MemberRole::Owner)
    }

    pub fn can_manage_mcp_connectors(&self) -> bool {
        matches!(self, MemberRole::Owner | MemberRole::Admin)
    }
}

impl std::fmt::Display for MemberRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemberRole::Owner => write!(f, "owner"),
            MemberRole::Admin => write!(f, "admin"),
            MemberRole::Member => write!(f, "member"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrganizationError {
    #[error("Organization not found")]
    NotFound,

    #[error("User not found")]
    UserNotFound,

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Invalid parameters: {0}")]
    InvalidParams(String),

    #[error("Organization already exists")]
    AlreadyExists,

    #[error("Organization is bound to a NEAR staking wallet")]
    StakingWalletBound,
    #[error("An organization that is a member's default cannot be deleted")]
    DefaultOrganization,

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("User is already a member")]
    AlreadyMember,
}

#[derive(Debug, Clone)]
pub struct CreateOrganizationRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AddOrganizationMemberRequest {
    pub user_id: Uuid,
    pub role: MemberRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOrganizationMemberResult {
    Removed,
    NotFound,
    Unauthorized,
    LastOwner,
}

/// Organization member with full user information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationMemberWithUser {
    pub organization_id: OrganizationId,
    pub user_id: UserId,
    pub role: MemberRole,
    pub joined_at: DateTime<Utc>,
    pub user: User,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationOrderBy {
    CreatedAt,
    JoinedAt,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationOrderDirection {
    Asc,
    Desc,
}

#[cfg_attr(any(test, feature = "test-mocks"), mockall::automock)]
#[async_trait]
pub trait OrganizationRepository: Send + Sync {
    async fn create(
        &self,
        request: CreateOrganizationRequest,
        creator_user_id: Uuid,
    ) -> Result<Organization, RepositoryError>;

    async fn get_by_id(&self, id: Uuid) -> Result<Option<Organization>, RepositoryError>;

    async fn get_by_name(&self, name: &str) -> Result<Option<Organization>, RepositoryError>;

    async fn get_member(
        &self,
        organization_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<OrganizationMember>, RepositoryError>;

    /// Atomically applies the organization-settings fields present in `patch`.
    /// Omitted fields are preserved and explicit nulls remove their JSON keys.
    /// `actor_user_id = None` is reserved for system-administrator operations.
    async fn patch_settings(
        &self,
        id: Uuid,
        patch: PatchOrganizationSettings,
        actor_user_id: Option<Uuid>,
    ) -> Result<Organization, RepositoryError>;

    async fn add_member(
        &self,
        org_id: Uuid,
        request: AddOrganizationMemberRequest,
        invited_by: Uuid,
    ) -> Result<OrganizationMember, RepositoryError>;

    async fn remove_member(
        &self,
        org_id: Uuid,
        user_id: Uuid,
        requester_user_id: Uuid,
    ) -> Result<RemoveOrganizationMemberResult, RepositoryError>;

    async fn list_members_paginated(
        &self,
        org_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationMember>, RepositoryError>;

    async fn get_member_count(&self, org_id: Uuid) -> Result<i64, RepositoryError>;

    async fn count_organizations_by_user(&self, user_id: Uuid) -> Result<i64, RepositoryError>;

    async fn list_organizations_by_user(
        &self,
        user_id: Uuid,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<Organization>, RepositoryError>;

    async fn list_organizations_with_roles_by_user(
        &self,
        user_id: Uuid,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<OrganizationWithRole>, RepositoryError>;
}

/// Service trait for organization operations
#[cfg_attr(any(test, feature = "test-mocks"), mockall::automock)]
#[async_trait]
pub trait OrganizationServiceTrait: Send + Sync {
    /// Create a new organization
    async fn create_organization(
        &self,
        name: String,
        description: Option<String>,
        owner_id: UserId,
    ) -> Result<Organization, OrganizationError>;

    /// Get an organization by ID
    async fn get_organization(&self, id: OrganizationId)
        -> Result<Organization, OrganizationError>;

    /// List organizations accessible to a user (where they are a member)
    async fn list_organizations_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<Organization>, OrganizationError>;

    async fn list_organizations_with_roles_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<OrganizationWithRole>, OrganizationError>;

    /// Count organizations accessible to a user
    async fn count_organizations_for_user(&self, user_id: UserId)
        -> Result<i64, OrganizationError>;

    /// Add a member to an organization
    async fn add_member(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError>;

    /// Check if a user is a member of an organization
    async fn is_member(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError>;

    /// Get a user's role in an organization
    async fn get_user_role(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<MemberRole>, OrganizationError>;

    /// Get the number of members in an organization
    async fn get_member_count(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, OrganizationError>;

    /// Get organization by name
    async fn get_organization_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Organization>, OrganizationError>;

    /// List organization members with full user information (paginated)
    async fn get_members_with_users_paginated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationMemberWithUser>, OrganizationError>;

    /// Get the effective organization settings for an organization member.
    async fn get_organization_settings(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<OrganizationSettings, OrganizationError>;

    /// Add a member by user ID (validates user exists)
    async fn add_member_validated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError>;

    /// Get the effective fallback policy from an admin-authenticated call path.
    async fn get_fallback_enabled_for_admin(
        &self,
        organization_id: OrganizationId,
    ) -> Result<bool, OrganizationError>;

    /// Set the fallback policy from an admin-authenticated call path.
    async fn update_fallback_enabled_for_admin(
        &self,
        organization_id: OrganizationId,
        fallback_enabled: bool,
    ) -> Result<bool, OrganizationError>;

    /// Get organization system prompt
    async fn get_system_prompt(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<String>, OrganizationError>;
}
