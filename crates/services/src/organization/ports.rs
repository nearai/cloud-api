use super::super::auth::ports::{User, UserId};
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
    pub id: OrganizationId,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: UserId,
    pub settings: serde_json::Value,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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

    pub fn can_manage_members(&self) -> bool {
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

    #[error("Internal error: {0}")]
    InternalError(String),

    #[error("User is already a member")]
    AlreadyMember,
}

#[derive(Debug, Clone)]
pub struct CreateOrganizationRequest {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateOrganizationRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub rate_limit: Option<i32>,
    pub settings: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct AddOrganizationMemberRequest {
    pub user_id: Uuid,
    pub role: MemberRole,
}

#[derive(Debug, Clone)]
pub struct UpdateOrganizationMemberRequest {
    pub role: MemberRole,
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

/// Result of a single invitation attempt
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvitationResult {
    pub email: String,
    pub success: bool,
    pub member: Option<OrganizationMember>,
    pub error: Option<String>,
}

/// Batch invitation response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchInvitationResponse {
    pub results: Vec<InvitationResult>,
    pub total: usize,
    pub successful: usize,
    pub failed: usize,
}

/// Organization invitation status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Declined,
    Expired,
}

/// Organization invitation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrganizationInvitation {
    pub id: Uuid,
    pub organization_id: OrganizationId,
    pub email: String,
    pub role: MemberRole,
    pub invited_by_user_id: UserId,
    pub status: InvitationStatus,
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub responded_at: Option<DateTime<Utc>>,
}

/// Create invitation request
#[derive(Debug, Clone)]
pub struct CreateInvitationRequest {
    pub email: String,
    pub role: MemberRole,
    pub expires_in_hours: i64,
}

#[async_trait]
pub trait OrganizationRepository: Send + Sync {
    async fn create(
        &self,
        request: CreateOrganizationRequest,
        creator_user_id: Uuid,
    ) -> Result<Organization>;

    async fn get_by_id(&self, id: Uuid) -> Result<Option<Organization>>;

    async fn get_by_name(&self, name: &str) -> Result<Option<Organization>>;

    async fn get_member(
        &self,
        organization_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<OrganizationMember>>;

    async fn update(&self, id: Uuid, request: UpdateOrganizationRequest) -> Result<Organization>;

    async fn delete(&self, id: Uuid) -> Result<bool>;

    async fn add_member(
        &self,
        org_id: Uuid,
        request: AddOrganizationMemberRequest,
        invited_by: Uuid,
    ) -> Result<OrganizationMember>;

    async fn update_member(
        &self,
        org_id: Uuid,
        user_id: Uuid,
        request: UpdateOrganizationMemberRequest,
    ) -> Result<OrganizationMember>;

    async fn remove_member(&self, org_id: Uuid, user_id: Uuid) -> Result<bool>;

    async fn list_members(&self, org_id: Uuid) -> Result<Vec<OrganizationMember>>;

    async fn get_member_count(&self, org_id: Uuid) -> Result<i64>;

    async fn list_organizations_by_user(
        &self,
        user_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Organization>>;
}

/// Repository trait for organization invitations
#[async_trait]
pub trait OrganizationInvitationRepository: Send + Sync {
    /// Create a new invitation
    async fn create(
        &self,
        org_id: Uuid,
        request: CreateInvitationRequest,
        invited_by: Uuid,
    ) -> Result<OrganizationInvitation>;

    /// Get invitation by ID
    async fn get_by_id(&self, id: Uuid) -> Result<Option<OrganizationInvitation>>;

    /// Get invitation by token
    async fn get_by_token(&self, token: &str) -> Result<Option<OrganizationInvitation>>;

    /// List invitations for an organization
    async fn list_by_organization(
        &self,
        org_id: Uuid,
        status: Option<InvitationStatus>,
    ) -> Result<Vec<OrganizationInvitation>>;

    /// List invitations for a user by email
    async fn list_by_email(
        &self,
        email: &str,
        status: Option<InvitationStatus>,
    ) -> Result<Vec<OrganizationInvitation>>;

    /// Update invitation status
    async fn update_status(
        &self,
        id: Uuid,
        status: InvitationStatus,
    ) -> Result<OrganizationInvitation>;

    /// Delete invitation
    async fn delete(&self, id: Uuid) -> Result<bool>;

    /// Mark expired invitations
    async fn mark_expired(&self) -> Result<usize>;
}
