pub mod ports;
use super::auth::ports::{UserId, UserRepository};
use super::common::RepositoryError;
use crate::email::{EmailDeliveryOutcome, EmailSender, InvitationEmail, NoopEmailSender};
use anyhow::Result;
use async_trait::async_trait;
pub use ports::*;
use std::sync::Arc;

pub struct OrganizationServiceImpl {
    repository: Arc<dyn OrganizationRepository>,
    user_repository: Arc<dyn UserRepository>,
    invitation_repository: Arc<dyn ports::OrganizationInvitationRepository>,
    email_sender: Arc<dyn EmailSender>,
    invitations_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct InvitationSenderDetails {
    name: Option<String>,
    email: Option<String>,
}

#[derive(Debug, Clone)]
struct InvitationEmailAttempt {
    email_sent: bool,
    error: Option<String>,
    updated_invitation: Option<ports::OrganizationInvitation>,
}

impl OrganizationServiceImpl {
    pub fn new(
        repository: Arc<dyn OrganizationRepository>,
        user_repository: Arc<dyn UserRepository>,
        invitation_repository: Arc<dyn ports::OrganizationInvitationRepository>,
    ) -> Self {
        Self::new_with_email_sender(
            repository,
            user_repository,
            invitation_repository,
            Arc::new(NoopEmailSender),
            None,
        )
    }

    pub fn new_with_email_sender(
        repository: Arc<dyn OrganizationRepository>,
        user_repository: Arc<dyn UserRepository>,
        invitation_repository: Arc<dyn ports::OrganizationInvitationRepository>,
        email_sender: Arc<dyn EmailSender>,
        invitations_url: Option<String>,
    ) -> Self {
        Self {
            repository,
            user_repository,
            invitation_repository,
            email_sender,
            invitations_url,
        }
    }

    /// Convert RepositoryError to OrganizationError
    fn map_repository_error(err: crate::common::RepositoryError) -> OrganizationError {
        match err {
            RepositoryError::AlreadyExists => OrganizationError::AlreadyExists,
            RepositoryError::NotFound(msg) => {
                OrganizationError::InternalError(format!("Resource not found: {msg}"))
            }
            RepositoryError::RequiredFieldMissing(field) => {
                OrganizationError::InvalidParams(format!("Required field is missing: {field}"))
            }
            RepositoryError::ForeignKeyViolation(msg) => {
                OrganizationError::InvalidParams(format!("Referenced entity does not exist: {msg}"))
            }
            RepositoryError::ValidationFailed(msg) => {
                OrganizationError::InvalidParams(format!("Validation failed: {msg}"))
            }
            RepositoryError::DependencyExists(msg) => OrganizationError::InvalidParams(format!(
                "Cannot delete due to dependencies: {msg}"
            )),
            RepositoryError::TransactionConflict => {
                OrganizationError::InternalError("Transaction conflict, please retry".to_string())
            }
            RepositoryError::ConnectionFailed(msg) => {
                OrganizationError::InternalError(format!("Database connection failed: {msg}"))
            }
            RepositoryError::AuthenticationFailed => {
                OrganizationError::InternalError("Database authentication failed".to_string())
            }
            RepositoryError::PoolError(err) => {
                OrganizationError::InternalError(format!("Database connection pool error: {err}"))
            }
            RepositoryError::DatabaseError(err) => {
                OrganizationError::InternalError(format!("Database operation failed: {err}"))
            }
            RepositoryError::DataConversionError(err) => {
                OrganizationError::InternalError(format!("Data conversion error: {err}"))
            }
        }
    }

    /// Create a new organization (private helper)
    async fn create_organization_impl(
        &self,
        name: String,
        description: Option<String>,
        owner_id: UserId,
    ) -> Result<Organization, OrganizationError> {
        // Validate input
        if name.trim().is_empty() {
            return Err(OrganizationError::InvalidParams(
                "Organization name cannot be empty".to_string(),
            ));
        }

        let request = CreateOrganizationRequest { name, description };

        self.repository
            .create(request, owner_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Get an organization by ID (private helper)
    async fn get_organization_impl(
        &self,
        id: OrganizationId,
    ) -> Result<Organization, OrganizationError> {
        self.repository
            .get_by_id(id.0)
            .await
            .map_err(Self::map_repository_error)?
            .ok_or(OrganizationError::NotFound)
    }

    /// Update an organization (private helper)
    async fn update_organization_impl(
        &self,
        id: OrganizationId,
        user_id: UserId,
        name: Option<String>,
        description: Option<String>,
        rate_limit: Option<i32>,
        settings: Option<serde_json::Value>,
    ) -> Result<Organization, OrganizationError> {
        // Check if user has permission
        let org = self.get_organization_impl(id.clone()).await?;
        if org.owner_id != user_id {
            // Check if user is admin
            if let Ok(Some(member)) = self.repository.get_member(id.0, user_id.0).await {
                if member.role != MemberRole::Owner && member.role != MemberRole::Admin {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners and admins can update organization".to_string(),
                    ));
                }
            } else {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ));
            }
        }

        // Validate name if provided
        if let Some(ref n) = name {
            if n.trim().is_empty() {
                return Err(OrganizationError::InvalidParams(
                    "Organization name cannot be empty".to_string(),
                ));
            }
        }

        let request = UpdateOrganizationRequest {
            name,
            description,
            rate_limit,
            settings,
        };

        self.repository
            .update(id.0, request)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Delete an organization (owner only, private helper)
    async fn delete_organization_impl(
        &self,
        id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if user is the owner
        let org = self.get_organization_impl(id.clone()).await?;
        if org.owner_id != user_id {
            return Err(OrganizationError::Unauthorized(
                "Only the owner can delete an organization".to_string(),
            ));
        }

        self.repository
            .delete(id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// List organizations accessible to a user (where they are a member, private helper)
    async fn list_organizations_for_user_impl(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<Organization>, OrganizationError> {
        self.repository
            .list_organizations_by_user(user_id.0, limit, offset, order_by, order_direction)
            .await
            .map_err(Self::map_repository_error)
    }

    async fn list_organizations_with_roles_for_user_impl(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<OrganizationWithRole>, OrganizationError> {
        self.repository
            .list_organizations_with_roles_by_user(
                user_id.0,
                limit,
                offset,
                order_by,
                order_direction,
            )
            .await
            .map_err(Self::map_repository_error)
    }

    /// Add a member to an organization (private helper)
    async fn add_member_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            // Check if user is admin
            if let Ok(Some(member)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                if member.role != MemberRole::Owner && member.role != MemberRole::Admin {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners and admins can add members".to_string(),
                    ));
                }
            } else {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ));
            }
        }

        // Can't add someone as owner through this method
        if role == MemberRole::Owner {
            return Err(OrganizationError::InvalidParams(
                "Cannot add a member as owner. Use transfer ownership instead.".to_string(),
            ));
        }

        let request = AddOrganizationMemberRequest {
            user_id: new_member_id.0,
            role,
        };

        self.repository
            .add_member(organization_id.0, request, requester_id.0)
            .await
            .map_err(|e| match e {
                RepositoryError::AlreadyExists => OrganizationError::AlreadyMember,
                _ => Self::map_repository_error(e),
            })
    }

    /// Remove a member from an organization (private helper)
    async fn remove_member_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization_impl(organization_id.clone()).await?;

        // Can't remove the owner
        if member_id == org.owner_id {
            return Err(OrganizationError::InvalidParams(
                "Cannot remove the owner from the organization".to_string(),
            ));
        }

        if org.owner_id != requester_id {
            // Check if user is admin
            if let Ok(Some(member)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                if member.role != MemberRole::Owner && member.role != MemberRole::Admin {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners and admins can remove members".to_string(),
                    ));
                }
            } else {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ));
            }
        }

        self.repository
            .remove_member(organization_id.0, member_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Update a member's role (private helper)
    async fn update_member_role_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
        new_role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization_impl(organization_id.clone()).await?;

        // Only owner can change roles
        if org.owner_id != requester_id {
            return Err(OrganizationError::Unauthorized(
                "Only the owner can change member roles".to_string(),
            ));
        }

        // Can't change owner's role through this method
        if member_id == org.owner_id {
            return Err(OrganizationError::InvalidParams(
                "Cannot change the owner's role. Use transfer ownership instead.".to_string(),
            ));
        }

        // Can't set someone as owner through this method
        if new_role == MemberRole::Owner {
            return Err(OrganizationError::InvalidParams(
                "Cannot set a member as owner. Use transfer ownership instead.".to_string(),
            ));
        }

        let request = UpdateOrganizationMemberRequest { role: new_role };

        self.repository
            .update_member(organization_id.0, member_id.0, request)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Check if a user is a member of an organization (private helper)
    async fn is_member_impl(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if user is owner
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id == user_id {
            return Ok(true);
        }

        // Check membership
        let member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?;

        Ok(member.is_some())
    }

    /// Get a user's role in an organization (private helper)
    async fn get_user_role_impl(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<MemberRole>, OrganizationError> {
        // Check if user is owner
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id == user_id {
            return Ok(Some(MemberRole::Owner));
        }

        // Check membership
        let member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?;

        Ok(member.map(|m| m.role))
    }

    /// Get the number of members in an organization (private helper)
    async fn get_member_count_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, OrganizationError> {
        // Check if requester is a member
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            if let Ok(Some(_)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                // User is a member, can view member count
            } else {
                return Err(OrganizationError::Unauthorized(
                    "Only members can view the member count".to_string(),
                ));
            }
        }

        self.repository
            .get_member_count(organization_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    /// Get organization by name (private helper)
    async fn get_organization_by_name_impl(
        &self,
        name: &str,
    ) -> Result<Option<Organization>, OrganizationError> {
        self.repository
            .get_by_name(name)
            .await
            .map_err(Self::map_repository_error)
    }

    /// List organization members with full user information (paginated, private helper)
    async fn get_members_with_users_paginated_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationMemberWithUser>, OrganizationError> {
        // Check if requester is a member
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            if let Ok(Some(_)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                // User is a member, can view member list
            } else {
                return Err(OrganizationError::Unauthorized(
                    "Only members can view the member list".to_string(),
                ));
            }
        }

        // Get members with pagination
        let members = self
            .repository
            .list_members_paginated(organization_id.0, limit, offset)
            .await
            .map_err(Self::map_repository_error)?;

        // Fetch user info for each member
        let mut members_with_users = Vec::new();
        for member in members {
            if let Ok(Some(user)) = self.user_repository.get_by_id(member.user_id.clone()).await {
                members_with_users.push(OrganizationMemberWithUser {
                    organization_id: member.organization_id,
                    user_id: member.user_id,
                    role: member.role,
                    joined_at: member.joined_at,
                    user,
                });
            }
        }

        Ok(members_with_users)
    }

    /// Invite members by email (batch operation, private helper)
    async fn invite_members_by_email_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        invitations: Vec<(String, MemberRole)>, // (email, role) pairs
    ) -> Result<BatchInvitationResponse, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            // Check if user is admin
            if let Ok(Some(member)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                if member.role != MemberRole::Owner && member.role != MemberRole::Admin {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners and admins can invite members".to_string(),
                    ));
                }
            } else {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ));
            }
        }

        let mut results = Vec::new();
        let mut successful = 0;
        let mut failed = 0;

        for (email, role) in invitations {
            // Lookup user by email
            let user_result = self.user_repository.get_by_email(&email).await;

            match user_result {
                Ok(Some(user)) => {
                    // Try to add the member
                    let request = AddOrganizationMemberRequest {
                        user_id: user.id.0,
                        role: role.clone(),
                    };

                    match self
                        .repository
                        .add_member(organization_id.0, request, requester_id.0)
                        .await
                    {
                        Ok(member) => {
                            successful += 1;
                            results.push(InvitationResult {
                                email,
                                success: true,
                                member: Some(member),
                                error: None,
                                email_sent: false,
                                email_error: None,
                            });
                        }
                        Err(e) => {
                            failed += 1;
                            let error_msg = if e.to_string().contains("already a member") {
                                "User is already a member".to_string()
                            } else {
                                "Failed to add member".to_string()
                            };
                            results.push(InvitationResult {
                                email,
                                success: false,
                                member: None,
                                error: Some(error_msg),
                                email_sent: false,
                                email_error: None,
                            });
                        }
                    }
                }
                Ok(None) => {
                    failed += 1;
                    results.push(InvitationResult {
                        email,
                        success: false,
                        member: None,
                        error: Some("User not found".to_string()),
                        email_sent: false,
                        email_error: None,
                    });
                }
                Err(e) => {
                    failed += 1;
                    results.push(InvitationResult {
                        email,
                        success: false,
                        member: None,
                        error: Some(format!("Failed to lookup user: {e}")),
                        email_sent: false,
                        email_error: None,
                    });
                }
            }
        }

        let total = results.len();
        Ok(BatchInvitationResponse {
            results,
            total,
            successful,
            failed,
        })
    }

    /// Add a member by user ID (validates user exists, private helper)
    async fn add_member_validated_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Verify user exists
        let _user = self
            .user_repository
            .get_by_id(new_member_id.clone())
            .await
            .map_err(|e| OrganizationError::InternalError(format!("Failed to verify user: {e}")))?
            .ok_or(OrganizationError::UserNotFound)?;

        // Use existing add_member logic
        self.add_member_impl(organization_id, requester_id, new_member_id, role)
            .await
    }

    /// Update member role with additional validation (private helper)
    async fn update_member_role_validated_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
        new_role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Check if requester has permission (only owner or admin can update roles)
        let org = self.get_organization_impl(organization_id.clone()).await?;

        let requester_member = if org.owner_id == requester_id {
            // Owner has Owner role implicitly
            Some(MemberRole::Owner)
        } else {
            self.repository
                .get_member(organization_id.0, requester_id.0)
                .await
                .map_err(|e| {
                    OrganizationError::InternalError(format!("Failed to check membership: {e}"))
                })?
                .map(|m| m.role)
        };

        match requester_member {
            Some(role) if role.can_manage_members() => {
                // Only owners can promote to owner
                if matches!(new_role, MemberRole::Owner) && !matches!(role, MemberRole::Owner) {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners can promote members to owner".to_string(),
                    ));
                }
            }
            _ => {
                return Err(OrganizationError::Unauthorized(
                    "Insufficient permissions to update member roles".to_string(),
                ));
            }
        }

        // Use existing update_member_role logic
        self.update_member_role_impl(organization_id, requester_id, member_id, new_role)
            .await
    }

    /// Remove member with last owner protection (private helper)
    async fn remove_member_validated_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if removing last owner
        let members = self
            .repository
            .list_members_paginated(organization_id.0, 1, 0)
            .await
            .map_err(Self::map_repository_error)?;

        let owner_count = members
            .iter()
            .filter(|m| matches!(m.role, MemberRole::Owner))
            .count();

        if owner_count == 1 {
            // Check if the member being removed is an owner
            if let Some(member) = members.iter().find(|m| m.user_id == member_id) {
                if matches!(member.role, MemberRole::Owner) {
                    return Err(OrganizationError::InvalidParams(
                        "Cannot remove the last owner from organization".to_string(),
                    ));
                }
            }
        }

        // Allow members to remove themselves (leave organization)
        let can_remove = if requester_id == member_id {
            true
        } else {
            // Check requester permissions
            let org = self.get_organization_impl(organization_id.clone()).await?;
            if org.owner_id == requester_id {
                true
            } else if let Ok(Some(member)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                member.role.can_manage_members()
            } else {
                false
            }
        };

        if !can_remove {
            return Err(OrganizationError::Unauthorized(
                "Insufficient permissions to remove member".to_string(),
            ));
        }

        self.repository
            .remove_member(organization_id.0, member_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    async fn send_invitation_email(
        &self,
        org: &Organization,
        invitation: &ports::OrganizationInvitation,
        sender_details: &InvitationSenderDetails,
    ) -> InvitationEmailAttempt {
        let Some(invitations_url) = self.invitations_url.clone() else {
            let updated_invitation = match self
                .invitation_repository
                .record_email_skipped(invitation.id)
                .await
            {
                Ok(invitation) => Some(invitation),
                Err(err) => {
                    tracing::warn!(
                        invitation_id = %invitation.id,
                        organization_id = %invitation.organization_id.0,
                        "Failed to record skipped invitation email status: {err}"
                    );
                    None
                }
            };
            return InvitationEmailAttempt {
                email_sent: false,
                error: Some("Invitation URL is not configured".to_string()),
                updated_invitation,
            };
        };

        let email = InvitationEmail {
            recipient_email: invitation.email.clone(),
            organization_name: org.name.clone(),
            role: invitation.role.to_string(),
            inviter_name: sender_details.name.clone(),
            inviter_email: sender_details.email.clone(),
            expires_at: invitation.expires_at,
            invitations_url,
        };

        match self.email_sender.send_invitation(&email).await {
            Ok(EmailDeliveryOutcome::Sent { message_id }) => {
                let updated_invitation = match self
                    .invitation_repository
                    .record_email_sent(invitation.id, message_id)
                    .await
                {
                    Ok(invitation) => Some(invitation),
                    Err(err) => {
                        tracing::warn!(
                            invitation_id = %invitation.id,
                            organization_id = %invitation.organization_id.0,
                            "Failed to record sent invitation email status: {err}"
                        );
                        None
                    }
                };
                InvitationEmailAttempt {
                    email_sent: true,
                    error: None,
                    updated_invitation,
                }
            }
            Ok(EmailDeliveryOutcome::Skipped) => {
                let updated_invitation = match self
                    .invitation_repository
                    .record_email_skipped(invitation.id)
                    .await
                {
                    Ok(invitation) => Some(invitation),
                    Err(err) => {
                        tracing::warn!(
                            invitation_id = %invitation.id,
                            organization_id = %invitation.organization_id.0,
                            "Failed to record skipped invitation email status: {err}"
                        );
                        None
                    }
                };
                InvitationEmailAttempt {
                    email_sent: false,
                    error: Some("Invitation email delivery was skipped".to_string()),
                    updated_invitation,
                }
            }
            Err(err) => {
                let sanitized_error = err.sanitized_message();
                let updated_invitation = match self
                    .invitation_repository
                    .record_email_failed(invitation.id, sanitized_error.clone())
                    .await
                {
                    Ok(invitation) => Some(invitation),
                    Err(record_err) => {
                        tracing::warn!(
                            invitation_id = %invitation.id,
                            organization_id = %invitation.organization_id.0,
                            "Failed to record failed invitation email status: {record_err}"
                        );
                        None
                    }
                };
                InvitationEmailAttempt {
                    email_sent: false,
                    error: Some(sanitized_error),
                    updated_invitation,
                }
            }
        }
    }

    async fn load_invitation_sender_details(
        &self,
        requester_id: &UserId,
    ) -> InvitationSenderDetails {
        match self.user_repository.get_by_id(requester_id.clone()).await {
            Ok(Some(user)) => InvitationSenderDetails {
                name: user.display_name.or(Some(user.username)),
                email: Some(user.email),
            },
            Ok(None) => InvitationSenderDetails::default(),
            Err(err) => {
                tracing::warn!(
                    inviter_id = %requester_id.0,
                    "Failed to load inviter details for invitation email: {err}"
                );
                InvitationSenderDetails::default()
            }
        }
    }

    async fn get_invitation_requester_role(
        &self,
        organization_id: &OrganizationId,
        requester_id: &UserId,
        org: &Organization,
    ) -> Result<MemberRole, OrganizationError> {
        if &org.owner_id == requester_id {
            return Ok(MemberRole::Owner);
        }

        let member = self
            .repository
            .get_member(organization_id.0, requester_id.0)
            .await
            .map_err(Self::map_repository_error)?
            .ok_or_else(|| {
                OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                )
            })?;

        if !member.role.can_manage_members() {
            return Err(OrganizationError::Unauthorized(
                "Only owners and admins can invite members".to_string(),
            ));
        }

        Ok(member.role)
    }

    /// Create invitations for users (supports unregistered users, private helper)
    async fn create_invitations_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        invitations: Vec<(String, MemberRole)>, // (email, role) pairs
        expires_in_hours: i64,
    ) -> Result<BatchInvitationResponse, OrganizationError> {
        let org = self.get_organization_impl(organization_id.clone()).await?;
        let requester_role = self
            .get_invitation_requester_role(&organization_id, &requester_id, &org)
            .await?;

        let mut results = Vec::new();
        let mut successful = 0;
        let mut failed = 0;
        let sender_details = if self.invitations_url.is_some() {
            self.load_invitation_sender_details(&requester_id).await
        } else {
            InvitationSenderDetails::default()
        };

        for (email, role) in invitations {
            if !requester_role.can_invite_as(&role) {
                failed += 1;
                results.push(ports::InvitationResult {
                    email,
                    success: false,
                    member: None,
                    error: Some(format!(
                        "Insufficient permissions to invite members as {role}"
                    )),
                    email_sent: false,
                    email_error: None,
                });
                continue;
            }

            // Check if user is already a member
            if let Ok(Some(user)) = self.user_repository.get_by_email(&email).await {
                if let Ok(Some(_)) = self
                    .repository
                    .get_member(organization_id.0, user.id.0)
                    .await
                {
                    failed += 1;
                    results.push(ports::InvitationResult {
                        email,
                        success: false,
                        member: None,
                        error: Some("User is already a member".to_string()),
                        email_sent: false,
                        email_error: None,
                    });
                    continue;
                }
            }

            // Create invitation
            let request = ports::CreateInvitationRequest {
                email: email.clone(),
                role: role.clone(),
                expires_in_hours,
            };

            match self
                .invitation_repository
                .create(organization_id.0, request, requester_id.0)
                .await
            {
                Ok(invitation) => {
                    let email_attempt = self
                        .send_invitation_email(&org, &invitation, &sender_details)
                        .await;
                    successful += 1;
                    results.push(ports::InvitationResult {
                        email,
                        success: true,
                        member: None,
                        error: None,
                        email_sent: email_attempt.email_sent,
                        email_error: email_attempt.error,
                    });
                }
                Err(e) => {
                    failed += 1;
                    results.push(ports::InvitationResult {
                        email,
                        success: false,
                        member: None,
                        error: Some(format!("Failed to create invitation: {e}")),
                        email_sent: false,
                        email_error: None,
                    });
                }
            }
        }

        let total = results.len();
        Ok(BatchInvitationResponse {
            results,
            total,
            successful,
            failed,
        })
    }

    /// List pending invitations for a user by email (private helper)
    async fn list_user_invitations_impl(
        &self,
        email: &str,
    ) -> Result<Vec<ports::OrganizationInvitationWithDetails>, OrganizationError> {
        self.invitation_repository
            .list_by_email_with_details(email, Some(ports::InvitationStatus::Pending))
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to list invitations: {e}"))
            })
    }

    /// Get invitation by token (public, for viewing before auth, private helper)
    async fn get_invitation_by_token_impl(
        &self,
        token: &str,
    ) -> Result<ports::OrganizationInvitation, OrganizationError> {
        let invitation = self
            .invitation_repository
            .get_by_token(token)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get invitation: {e}"))
            })?
            .ok_or(OrganizationError::NotFound)?;

        // Check if already expired (but don't auto-update here for GET)
        if invitation.status != ports::InvitationStatus::Pending {
            return Err(OrganizationError::InvalidParams(format!(
                "Invitation is {}, not pending",
                match invitation.status {
                    ports::InvitationStatus::Accepted => "already accepted",
                    ports::InvitationStatus::Declined => "declined",
                    ports::InvitationStatus::Expired => "expired",
                    _ => "not available",
                }
            )));
        }

        if invitation.expires_at < chrono::Utc::now() {
            return Err(OrganizationError::InvalidParams(
                "Invitation has expired".to_string(),
            ));
        }

        Ok(invitation)
    }

    /// Accept invitation by token (private helper)
    async fn accept_invitation_by_token_impl(
        &self,
        token: &str,
        user_id: UserId,
        user_email: &str,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Get invitation by token
        let invitation = self
            .invitation_repository
            .get_by_token(token)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get invitation: {e}"))
            })?
            .ok_or(OrganizationError::NotFound)?;

        // Use the existing accept_invitation logic with the invitation ID
        self.accept_invitation_impl(invitation.id, user_id, user_email)
            .await
    }

    /// Accept an invitation (creates membership if user is registered, private helper)
    async fn accept_invitation_impl(
        &self,
        invitation_id: uuid::Uuid,
        user_id: UserId,
        user_email: &str,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Get invitation
        let invitation = self
            .invitation_repository
            .get_by_id(invitation_id)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get invitation: {e}"))
            })?
            .ok_or(OrganizationError::NotFound)?;

        // Verify the invitation belongs to this user
        if invitation.email.to_lowercase() != user_email.to_lowercase() {
            return Err(OrganizationError::Unauthorized(
                "Invitation does not belong to this user".to_string(),
            ));
        }

        // Check if invitation is still valid
        if invitation.status != ports::InvitationStatus::Pending {
            return Err(OrganizationError::InvalidParams(
                "Invitation is not pending".to_string(),
            ));
        }

        if invitation.expires_at < chrono::Utc::now() {
            // Mark as expired
            let _ = self
                .invitation_repository
                .update_status(invitation_id, ports::InvitationStatus::Expired)
                .await;
            return Err(OrganizationError::InvalidParams(
                "Invitation has expired".to_string(),
            ));
        }

        // Check if already a member
        if let Ok(Some(_)) = self
            .repository
            .get_member(invitation.organization_id.0, user_id.0)
            .await
        {
            // Mark invitation as accepted anyway
            let _ = self
                .invitation_repository
                .update_status(invitation_id, ports::InvitationStatus::Accepted)
                .await;
            return Err(OrganizationError::AlreadyMember);
        }

        // Add user as member
        let add_request = AddOrganizationMemberRequest {
            user_id: user_id.0,
            role: invitation.role.clone(),
        };

        let member = self
            .repository
            .add_member(
                invitation.organization_id.0,
                add_request,
                invitation.invited_by_user_id.0,
            )
            .await
            .map_err(|e| OrganizationError::InternalError(format!("Failed to add member: {e}")))?;

        // Mark invitation as accepted
        self.invitation_repository
            .update_status(invitation_id, ports::InvitationStatus::Accepted)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to update invitation: {e}"))
            })?;

        Ok(member)
    }

    /// Decline an invitation (private helper)
    async fn decline_invitation_impl(
        &self,
        invitation_id: uuid::Uuid,
        user_email: &str,
    ) -> Result<(), OrganizationError> {
        // Get invitation
        let invitation = self
            .invitation_repository
            .get_by_id(invitation_id)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get invitation: {e}"))
            })?
            .ok_or(OrganizationError::NotFound)?;

        // Verify the invitation belongs to this user
        if invitation.email.to_lowercase() != user_email.to_lowercase() {
            return Err(OrganizationError::Unauthorized(
                "Invitation does not belong to this user".to_string(),
            ));
        }

        // Check if invitation is still valid
        if invitation.status != ports::InvitationStatus::Pending {
            return Err(OrganizationError::InvalidParams(
                "Invitation is not pending".to_string(),
            ));
        }

        // Mark invitation as declined
        self.invitation_repository
            .update_status(invitation_id, ports::InvitationStatus::Declined)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to update invitation: {e}"))
            })?;

        Ok(())
    }

    /// List invitations for an organization (admin/owner only, private helper)
    async fn list_organization_invitations_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        status: Option<ports::InvitationStatus>,
    ) -> Result<Vec<ports::OrganizationInvitation>, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization_impl(organization_id.clone()).await?;
        if org.owner_id != requester_id {
            if let Ok(Some(member)) = self
                .repository
                .get_member(organization_id.0, requester_id.0)
                .await
            {
                if !member.role.can_manage_members() {
                    return Err(OrganizationError::Unauthorized(
                        "Only owners and admins can view invitations".to_string(),
                    ));
                }
            } else {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ));
            }
        }

        self.invitation_repository
            .list_by_organization(organization_id.0, status)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to list invitations: {e}"))
            })
    }

    async fn list_invitation_email_deliveries_impl(
        &self,
        mut filters: ports::InvitationEmailDeliveryFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<ports::OrganizationInvitationEmailDelivery>, i64), OrganizationError> {
        if limit < 0 || offset < 0 {
            return Err(OrganizationError::InvalidParams(
                "limit and offset must be non-negative".to_string(),
            ));
        }

        if let (Some(created_after), Some(created_before)) =
            (filters.created_after, filters.created_before)
        {
            if created_after > created_before {
                return Err(OrganizationError::InvalidParams(
                    "created_after must be before created_before".to_string(),
                ));
            }
        }

        filters.recipient_email = filters
            .recipient_email
            .map(|email| email.trim().to_string())
            .filter(|email| !email.is_empty());

        self.invitation_repository
            .list_email_deliveries(filters, limit, offset)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!(
                    "Failed to list invitation email deliveries: {e}"
                ))
            })
    }

    async fn resend_invitation_email_impl(
        &self,
        invitation_id: uuid::Uuid,
    ) -> Result<ports::InvitationEmailResendResult, OrganizationError> {
        let invitation = self
            .invitation_repository
            .get_by_id(invitation_id)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get invitation: {e}"))
            })?
            .ok_or(OrganizationError::NotFound)?;

        if invitation.status != ports::InvitationStatus::Pending {
            return Err(OrganizationError::InvalidParams(
                "Invitation is not pending".to_string(),
            ));
        }

        if invitation.expires_at < chrono::Utc::now() {
            self.invitation_repository
                .update_status(invitation_id, ports::InvitationStatus::Expired)
                .await
                .map_err(|e| {
                    OrganizationError::InternalError(format!(
                        "Failed to mark invitation expired: {e}"
                    ))
                })?;

            return Err(OrganizationError::InvalidParams(
                "Invitation has expired".to_string(),
            ));
        }

        let org = self
            .get_organization_impl(invitation.organization_id.clone())
            .await?;
        let sender_details = self
            .load_invitation_sender_details(&invitation.invited_by_user_id)
            .await;
        let email_attempt = self
            .send_invitation_email(&org, &invitation, &sender_details)
            .await;

        let updated_invitation = match email_attempt.updated_invitation {
            Some(invitation) => invitation,
            None => self
                .invitation_repository
                .get_by_id(invitation_id)
                .await
                .map_err(|e| {
                    OrganizationError::InternalError(format!(
                        "Failed to reload invitation after resend: {e}"
                    ))
                })?
                .ok_or(OrganizationError::NotFound)?,
        };

        tracing::info!(
            invitation_id = %updated_invitation.id,
            organization_id = %updated_invitation.organization_id.0,
            email_status = ?updated_invitation.email_status,
            email_sent = email_attempt.email_sent,
            "Admin invitation email resend attempted"
        );

        Ok(ports::InvitationEmailResendResult {
            invitation_id: updated_invitation.id,
            recipient_email: updated_invitation.email,
            success: email_attempt.email_sent && email_attempt.error.is_none(),
            email_sent: email_attempt.email_sent,
            email_status: updated_invitation.email_status,
            email_sent_at: updated_invitation.email_sent_at,
            email_message_id: updated_invitation.email_message_id,
            email_last_error: updated_invitation.email_last_error,
            error: email_attempt.error,
        })
    }
}

// Implement the trait for the service
#[async_trait]
impl OrganizationServiceTrait for OrganizationServiceImpl {
    async fn create_organization(
        &self,
        name: String,
        description: Option<String>,
        owner_id: UserId,
    ) -> Result<Organization, OrganizationError> {
        self.create_organization_impl(name, description, owner_id)
            .await
    }

    async fn get_organization(
        &self,
        id: OrganizationId,
    ) -> Result<Organization, OrganizationError> {
        self.get_organization_impl(id).await
    }

    async fn update_organization(
        &self,
        id: OrganizationId,
        user_id: UserId,
        name: Option<String>,
        description: Option<String>,
        rate_limit: Option<i32>,
        settings: Option<serde_json::Value>,
    ) -> Result<Organization, OrganizationError> {
        self.update_organization_impl(id, user_id, name, description, rate_limit, settings)
            .await
    }

    async fn delete_organization(
        &self,
        id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        self.delete_organization_impl(id, user_id).await
    }

    async fn list_organizations_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<Organization>, OrganizationError> {
        self.list_organizations_for_user_impl(user_id, limit, offset, order_by, order_direction)
            .await
    }

    async fn list_organizations_with_roles_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<OrganizationWithRole>, OrganizationError> {
        self.list_organizations_with_roles_for_user_impl(
            user_id,
            limit,
            offset,
            order_by,
            order_direction,
        )
        .await
    }

    async fn count_organizations_for_user(
        &self,
        user_id: UserId,
    ) -> Result<i64, OrganizationError> {
        self.repository
            .count_organizations_by_user(user_id.0)
            .await
            .map_err(Self::map_repository_error)
    }

    async fn add_member(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        self.add_member_impl(organization_id, requester_id, new_member_id, role)
            .await
    }

    async fn remove_member(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
    ) -> Result<bool, OrganizationError> {
        self.remove_member_impl(organization_id, requester_id, member_id)
            .await
    }

    async fn update_member_role(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
        new_role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        self.update_member_role_impl(organization_id, requester_id, member_id, new_role)
            .await
    }

    async fn is_member(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        self.is_member_impl(organization_id, user_id).await
    }

    async fn get_user_role(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<MemberRole>, OrganizationError> {
        self.get_user_role_impl(organization_id, user_id).await
    }

    async fn get_member_count(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, OrganizationError> {
        self.get_member_count_impl(organization_id, requester_id)
            .await
    }

    async fn get_organization_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Organization>, OrganizationError> {
        self.get_organization_by_name_impl(name).await
    }

    async fn get_members_with_users_paginated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationMemberWithUser>, OrganizationError> {
        self.get_members_with_users_paginated_impl(organization_id, requester_id, limit, offset)
            .await
    }

    async fn invite_members_by_email(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        invitations: Vec<(String, MemberRole)>,
    ) -> Result<BatchInvitationResponse, OrganizationError> {
        self.invite_members_by_email_impl(organization_id, requester_id, invitations)
            .await
    }

    async fn add_member_validated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        self.add_member_validated_impl(organization_id, requester_id, new_member_id, role)
            .await
    }

    async fn update_member_role_validated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
        new_role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        self.update_member_role_validated_impl(organization_id, requester_id, member_id, new_role)
            .await
    }

    async fn remove_member_validated(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
    ) -> Result<bool, OrganizationError> {
        self.remove_member_validated_impl(organization_id, requester_id, member_id)
            .await
    }

    async fn create_invitations(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        invitations: Vec<(String, MemberRole)>,
        expires_in_hours: i64,
    ) -> Result<BatchInvitationResponse, OrganizationError> {
        self.create_invitations_impl(organization_id, requester_id, invitations, expires_in_hours)
            .await
    }

    async fn list_user_invitations(
        &self,
        email: &str,
    ) -> Result<Vec<OrganizationInvitationWithDetails>, OrganizationError> {
        self.list_user_invitations_impl(email).await
    }

    async fn get_invitation_by_token(
        &self,
        token: &str,
    ) -> Result<OrganizationInvitation, OrganizationError> {
        self.get_invitation_by_token_impl(token).await
    }

    async fn accept_invitation_by_token(
        &self,
        token: &str,
        user_id: UserId,
        user_email: &str,
    ) -> Result<OrganizationMember, OrganizationError> {
        self.accept_invitation_by_token_impl(token, user_id, user_email)
            .await
    }

    async fn accept_invitation(
        &self,
        invitation_id: uuid::Uuid,
        user_id: UserId,
        user_email: &str,
    ) -> Result<OrganizationMember, OrganizationError> {
        self.accept_invitation_impl(invitation_id, user_id, user_email)
            .await
    }

    async fn decline_invitation(
        &self,
        invitation_id: uuid::Uuid,
        user_email: &str,
    ) -> Result<(), OrganizationError> {
        self.decline_invitation_impl(invitation_id, user_email)
            .await
    }

    async fn list_organization_invitations(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        status: Option<InvitationStatus>,
    ) -> Result<Vec<OrganizationInvitation>, OrganizationError> {
        self.list_organization_invitations_impl(organization_id, requester_id, status)
            .await
    }

    async fn list_invitation_email_deliveries(
        &self,
        filters: InvitationEmailDeliveryFilters,
        limit: i64,
        offset: i64,
    ) -> Result<(Vec<OrganizationInvitationEmailDelivery>, i64), OrganizationError> {
        self.list_invitation_email_deliveries_impl(filters, limit, offset)
            .await
    }

    async fn resend_invitation_email(
        &self,
        invitation_id: uuid::Uuid,
    ) -> Result<InvitationEmailResendResult, OrganizationError> {
        self.resend_invitation_email_impl(invitation_id).await
    }

    async fn get_system_prompt(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<String>, OrganizationError> {
        // Check if user is a member of the organization
        let is_member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?
            .is_some();

        if !is_member {
            return Err(OrganizationError::Unauthorized(
                "User is not a member of this organization".to_string(),
            ));
        }

        // Get organization and extract system prompt from settings
        let org = self.get_organization_impl(organization_id).await?;

        let system_prompt = org
            .settings
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(system_prompt)
    }

    async fn update_system_prompt(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
        system_prompt: Option<String>,
    ) -> Result<Option<String>, OrganizationError> {
        // Check if user has permission to manage the organization
        let member = self
            .repository
            .get_member(organization_id.0, user_id.0)
            .await
            .map_err(Self::map_repository_error)?;

        let role = match member {
            Some(m) => m.role,
            None => {
                return Err(OrganizationError::Unauthorized(
                    "User is not a member of this organization".to_string(),
                ))
            }
        };

        if !role.can_manage_organization() {
            return Err(OrganizationError::Unauthorized(
                "Insufficient permissions to manage organization settings".to_string(),
            ));
        }

        // Get current organization
        let org = self.get_organization_impl(organization_id.clone()).await?;

        // Update settings with new system prompt
        let mut settings = if org.settings.is_object() {
            org.settings.clone()
        } else {
            // Initialize as empty object if not already an object
            serde_json::json!({})
        };

        if let Some(ref prompt) = system_prompt {
            // Set system prompt if provided
            if let Some(obj) = settings.as_object_mut() {
                obj.insert("system_prompt".to_string(), serde_json::json!(prompt));
            }
        } else {
            // Remove system prompt if None
            if let Some(obj) = settings.as_object_mut() {
                obj.remove("system_prompt");
            }
        }

        // Update organization with new settings
        let request = UpdateOrganizationRequest {
            name: None,
            description: None,
            rate_limit: None,
            settings: Some(settings),
        };

        self.repository
            .update(organization_id.0, request)
            .await
            .map_err(Self::map_repository_error)?;

        Ok(system_prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ports::{User, UserRole};
    use crate::email::{EmailDeliveryOutcome, EmailError};
    use std::sync::Mutex;
    use uuid::Uuid;

    struct StubOrgRepo {
        org: Organization,
        member: Option<OrganizationMember>,
    }

    #[async_trait]
    impl OrganizationRepository for StubOrgRepo {
        async fn create(
            &self,
            _: CreateOrganizationRequest,
            _: Uuid,
        ) -> Result<Organization, RepositoryError> {
            unimplemented!()
        }

        async fn get_by_id(&self, _: Uuid) -> Result<Option<Organization>, RepositoryError> {
            Ok(Some(self.org.clone()))
        }

        async fn get_by_name(&self, _: &str) -> Result<Option<Organization>, RepositoryError> {
            unimplemented!()
        }

        async fn get_member(
            &self,
            organization_id: Uuid,
            user_id: Uuid,
        ) -> Result<Option<OrganizationMember>, RepositoryError> {
            Ok(self
                .member
                .as_ref()
                .filter(|member| {
                    member.organization_id.0 == organization_id && member.user_id.0 == user_id
                })
                .cloned())
        }

        async fn update(
            &self,
            _: Uuid,
            _: UpdateOrganizationRequest,
        ) -> Result<Organization, RepositoryError> {
            unimplemented!()
        }

        async fn delete(&self, _: Uuid) -> Result<bool, RepositoryError> {
            unimplemented!()
        }

        async fn add_member(
            &self,
            _: Uuid,
            _: AddOrganizationMemberRequest,
            _: Uuid,
        ) -> Result<OrganizationMember, RepositoryError> {
            unimplemented!()
        }

        async fn update_member(
            &self,
            _: Uuid,
            _: Uuid,
            _: UpdateOrganizationMemberRequest,
        ) -> Result<OrganizationMember, RepositoryError> {
            unimplemented!()
        }

        async fn remove_member(&self, _: Uuid, _: Uuid) -> Result<bool, RepositoryError> {
            unimplemented!()
        }

        async fn list_members_paginated(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
        ) -> Result<Vec<OrganizationMember>, RepositoryError> {
            unimplemented!()
        }

        async fn get_member_count(&self, _: Uuid) -> Result<i64, RepositoryError> {
            unimplemented!()
        }

        async fn count_organizations_by_user(&self, _: Uuid) -> Result<i64, RepositoryError> {
            unimplemented!()
        }

        async fn list_organizations_by_user(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<Organization>, RepositoryError> {
            unimplemented!()
        }

        async fn list_organizations_with_roles_by_user(
            &self,
            _: Uuid,
            _: i64,
            _: i64,
            _: Option<OrganizationOrderBy>,
            _: Option<OrganizationOrderDirection>,
        ) -> Result<Vec<OrganizationWithRole>, RepositoryError> {
            unimplemented!()
        }
    }

    struct StubUserRepo {
        inviter: User,
        get_by_id_calls: Mutex<usize>,
    }

    #[async_trait]
    impl UserRepository for StubUserRepo {
        async fn create(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }

        async fn create_from_oauth(
            &self,
            _: String,
            _: String,
            _: Option<String>,
            _: Option<String>,
            _: String,
            _: String,
        ) -> anyhow::Result<User> {
            unimplemented!()
        }

        async fn get_by_id(&self, _: UserId) -> anyhow::Result<Option<User>> {
            *self.get_by_id_calls.lock().unwrap() += 1;
            Ok(Some(self.inviter.clone()))
        }

        async fn get_by_email(&self, _: &str) -> anyhow::Result<Option<User>> {
            Ok(None)
        }

        async fn get_by_provider(&self, _: &str, _: &str) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }

        async fn update_email(&self, _: UserId, _: String) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn update(
            &self,
            _: UserId,
            _: Option<String>,
            _: Option<String>,
        ) -> anyhow::Result<Option<User>> {
            unimplemented!()
        }

        async fn update_last_login(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn update_tokens_revoked_at(&self, _: UserId) -> anyhow::Result<()> {
            unimplemented!()
        }

        async fn delete(&self, _: UserId) -> anyhow::Result<bool> {
            unimplemented!()
        }

        async fn list(&self, _: i64, _: i64) -> anyhow::Result<Vec<User>> {
            unimplemented!()
        }
    }

    struct StubInvitationRepo {
        records: Mutex<Vec<OrganizationInvitation>>,
    }

    impl StubInvitationRepo {
        fn latest(&self, id: Uuid) -> OrganizationInvitation {
            self.records
                .lock()
                .unwrap()
                .iter()
                .find(|invitation| invitation.id == id)
                .unwrap()
                .clone()
        }

        fn update_email_status(
            &self,
            id: Uuid,
            email_status: InvitationEmailStatus,
            email_last_error: Option<String>,
            email_message_id: Option<String>,
        ) -> OrganizationInvitation {
            let mut records = self.records.lock().unwrap();
            let invitation = records
                .iter_mut()
                .find(|invitation| invitation.id == id)
                .unwrap();
            invitation.email_status = email_status;
            invitation.email_sent_at = if invitation.email_status == InvitationEmailStatus::Sent {
                Some(chrono::Utc::now())
            } else {
                None
            };
            invitation.email_last_error = email_last_error;
            invitation.email_message_id = email_message_id;
            invitation.clone()
        }
    }

    #[async_trait]
    impl OrganizationInvitationRepository for StubInvitationRepo {
        async fn create(
            &self,
            org_id: Uuid,
            request: CreateInvitationRequest,
            invited_by: Uuid,
        ) -> anyhow::Result<OrganizationInvitation> {
            let invitation = OrganizationInvitation {
                id: Uuid::new_v4(),
                organization_id: OrganizationId(org_id),
                email: request.email,
                role: request.role,
                invited_by_user_id: UserId(invited_by),
                status: InvitationStatus::Pending,
                token: "token".to_string(),
                created_at: chrono::Utc::now(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(request.expires_in_hours),
                responded_at: None,
                email_status: InvitationEmailStatus::NotAttempted,
                email_sent_at: None,
                email_last_error: None,
                email_message_id: None,
            };
            self.records.lock().unwrap().push(invitation.clone());
            Ok(invitation)
        }

        async fn get_by_id(&self, id: Uuid) -> anyhow::Result<Option<OrganizationInvitation>> {
            Ok(Some(self.latest(id)))
        }

        async fn get_by_token(&self, _: &str) -> anyhow::Result<Option<OrganizationInvitation>> {
            unimplemented!()
        }

        async fn list_by_organization(
            &self,
            _: Uuid,
            _: Option<InvitationStatus>,
        ) -> anyhow::Result<Vec<OrganizationInvitation>> {
            unimplemented!()
        }

        async fn list_by_email(
            &self,
            _: &str,
            _: Option<InvitationStatus>,
        ) -> anyhow::Result<Vec<OrganizationInvitation>> {
            unimplemented!()
        }

        async fn list_by_email_with_details(
            &self,
            _: &str,
            _: Option<InvitationStatus>,
        ) -> anyhow::Result<Vec<OrganizationInvitationWithDetails>> {
            unimplemented!()
        }

        async fn list_email_deliveries(
            &self,
            _: InvitationEmailDeliveryFilters,
            _: i64,
            _: i64,
        ) -> anyhow::Result<(Vec<OrganizationInvitationEmailDelivery>, i64)> {
            unimplemented!()
        }

        async fn update_status(
            &self,
            id: Uuid,
            status: InvitationStatus,
        ) -> anyhow::Result<OrganizationInvitation> {
            let mut records = self.records.lock().unwrap();
            let invitation = records
                .iter_mut()
                .find(|invitation| invitation.id == id)
                .unwrap();
            invitation.status = status;
            Ok(invitation.clone())
        }

        async fn record_email_sent(
            &self,
            id: Uuid,
            message_id: Option<String>,
        ) -> anyhow::Result<OrganizationInvitation> {
            Ok(self.update_email_status(id, InvitationEmailStatus::Sent, None, message_id))
        }

        async fn record_email_failed(
            &self,
            id: Uuid,
            error: String,
        ) -> anyhow::Result<OrganizationInvitation> {
            Ok(self.update_email_status(id, InvitationEmailStatus::Failed, Some(error), None))
        }

        async fn record_email_skipped(&self, id: Uuid) -> anyhow::Result<OrganizationInvitation> {
            Ok(self.update_email_status(id, InvitationEmailStatus::Skipped, None, None))
        }

        async fn delete(&self, _: Uuid) -> anyhow::Result<bool> {
            unimplemented!()
        }

        async fn mark_expired(&self) -> anyhow::Result<usize> {
            unimplemented!()
        }
    }

    struct StubEmailSender {
        outcome: Result<EmailDeliveryOutcome, EmailError>,
        sent_to: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EmailSender for StubEmailSender {
        async fn send_invitation(
            &self,
            email: &InvitationEmail,
        ) -> Result<EmailDeliveryOutcome, EmailError> {
            self.sent_to
                .lock()
                .unwrap()
                .push(email.recipient_email.clone());
            self.outcome.clone()
        }
    }

    fn make_service(
        outcome: Result<EmailDeliveryOutcome, EmailError>,
        invitations_url: Option<String>,
    ) -> (
        OrganizationServiceImpl,
        Arc<StubInvitationRepo>,
        Arc<StubEmailSender>,
        Arc<StubUserRepo>,
    ) {
        make_service_with_requester_role(outcome, invitations_url, MemberRole::Owner)
    }

    fn make_service_with_requester_role(
        outcome: Result<EmailDeliveryOutcome, EmailError>,
        invitations_url: Option<String>,
        requester_role: MemberRole,
    ) -> (
        OrganizationServiceImpl,
        Arc<StubInvitationRepo>,
        Arc<StubEmailSender>,
        Arc<StubUserRepo>,
    ) {
        let owner_id = UserId(Uuid::new_v4());
        let requester_id = if requester_role == MemberRole::Owner {
            owner_id.clone()
        } else {
            UserId(Uuid::new_v4())
        };
        let org_id = OrganizationId(Uuid::new_v4());
        let org = Organization {
            id: org_id.clone(),
            name: "Example Org".to_string(),
            description: None,
            owner_id: owner_id.clone(),
            settings: serde_json::json!({}),
            is_active: true,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let member = if requester_role == MemberRole::Owner {
            None
        } else {
            Some(OrganizationMember {
                organization_id: org_id,
                user_id: requester_id.clone(),
                role: requester_role.clone(),
                joined_at: chrono::Utc::now(),
            })
        };
        let inviter = User {
            id: requester_id,
            email: format!("{requester_role}@example.com"),
            username: requester_role.to_string(),
            display_name: Some(requester_role.to_string()),
            avatar_url: None,
            auth_provider: "test".to_string(),
            role: UserRole::User,
            is_active: true,
            last_login: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            tokens_revoked_at: None,
        };
        let invitation_repo = Arc::new(StubInvitationRepo {
            records: Mutex::new(Vec::new()),
        });
        let email_sender = Arc::new(StubEmailSender {
            outcome,
            sent_to: Mutex::new(Vec::new()),
        });
        let user_repo = Arc::new(StubUserRepo {
            inviter,
            get_by_id_calls: Mutex::new(0),
        });
        let service = OrganizationServiceImpl::new_with_email_sender(
            Arc::new(StubOrgRepo { org, member }) as Arc<dyn OrganizationRepository>,
            user_repo.clone() as Arc<dyn UserRepository>,
            invitation_repo.clone() as Arc<dyn OrganizationInvitationRepository>,
            email_sender.clone() as Arc<dyn EmailSender>,
            invitations_url,
        );

        (service, invitation_repo, email_sender, user_repo)
    }

    #[tokio::test]
    async fn create_invitations_records_sent_email_status() {
        let (service, invitation_repo, email_sender, user_repo) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();

        let response = service
            .create_invitations(
                org.id,
                org.owner_id,
                vec![("invitee@example.com".to_string(), MemberRole::Admin)],
                168,
            )
            .await
            .unwrap();

        assert_eq!(response.successful, 1);
        assert!(response.results[0].email_sent);
        assert_eq!(response.results[0].email_error, None);
        assert_eq!(
            email_sender.sent_to.lock().unwrap().as_slice(),
            &["invitee@example.com".to_string()]
        );
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.email_status, InvitationEmailStatus::Sent);
        assert_eq!(stored.email_message_id.as_deref(), Some("resend-email-id"));
        assert_eq!(*user_repo.get_by_id_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn create_invitations_loads_inviter_once_for_batch() {
        let (service, _, email_sender, user_repo) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();

        let response = service
            .create_invitations(
                org.id,
                org.owner_id,
                vec![
                    ("one@example.com".to_string(), MemberRole::Admin),
                    ("two@example.com".to_string(), MemberRole::Member),
                ],
                168,
            )
            .await
            .unwrap();

        assert_eq!(response.successful, 2);
        assert_eq!(
            email_sender.sent_to.lock().unwrap().as_slice(),
            &["one@example.com".to_string(), "two@example.com".to_string()]
        );
        assert_eq!(*user_repo.get_by_id_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn create_invitations_rejects_roles_above_requester_role() {
        let (service, invitation_repo, email_sender, user_repo) = make_service_with_requester_role(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            None,
            MemberRole::Admin,
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();

        let response = service
            .create_invitations(
                org.id,
                user_repo.inviter.id.clone(),
                vec![
                    ("owner@example.com".to_string(), MemberRole::Owner),
                    ("admin@example.com".to_string(), MemberRole::Admin),
                    ("member@example.com".to_string(), MemberRole::Member),
                ],
                168,
            )
            .await
            .unwrap();

        assert_eq!(response.total, 3);
        assert_eq!(response.successful, 2);
        assert_eq!(response.failed, 1);
        assert_eq!(response.results[0].email, "owner@example.com");
        assert!(!response.results[0].success);
        assert_eq!(
            response.results[0].error.as_deref(),
            Some("Insufficient permissions to invite members as owner")
        );
        assert!(response.results[1].success);
        assert!(response.results[2].success);
        assert!(email_sender.sent_to.lock().unwrap().is_empty());

        let records = invitation_repo.records.lock().unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|invitation| {
            invitation.role == MemberRole::Admin || invitation.role == MemberRole::Member
        }));
    }

    #[tokio::test]
    async fn create_invitations_keeps_invite_when_email_fails() {
        let (service, invitation_repo, _, _) = make_service(
            Err(EmailError::new("Resend failed\nwith details")),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();

        let response = service
            .create_invitations(
                org.id,
                org.owner_id,
                vec![("invitee@example.com".to_string(), MemberRole::Member)],
                168,
            )
            .await
            .unwrap();

        assert_eq!(response.successful, 1);
        assert!(response.results[0].success);
        assert!(!response.results[0].email_sent);
        assert_eq!(
            response.results[0].email_error.as_deref(),
            Some("Resend failed with details")
        );
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.email_status, InvitationEmailStatus::Failed);
        assert_eq!(
            stored.email_last_error.as_deref(),
            Some("Resend failed with details")
        );
    }

    #[tokio::test]
    async fn create_invitations_skips_email_without_invitations_url() {
        let (service, invitation_repo, email_sender, user_repo) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            None,
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();

        let response = service
            .create_invitations(
                org.id,
                org.owner_id,
                vec![("invitee@example.com".to_string(), MemberRole::Member)],
                168,
            )
            .await
            .unwrap();

        assert_eq!(response.successful, 1);
        assert!(!response.results[0].email_sent);
        assert_eq!(
            response.results[0].email_error.as_deref(),
            Some("Invitation URL is not configured")
        );
        assert!(email_sender.sent_to.lock().unwrap().is_empty());
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.email_status, InvitationEmailStatus::Skipped);
        assert_eq!(*user_repo.get_by_id_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn resend_invitation_email_records_sent_email_status() {
        let (service, invitation_repo, email_sender, _) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();
        let invitation = invitation_repo
            .create(
                org.id.0,
                CreateInvitationRequest {
                    email: "invitee@example.com".to_string(),
                    role: MemberRole::Member,
                    expires_in_hours: 168,
                },
                org.owner_id.0,
            )
            .await
            .unwrap();

        let result = service
            .resend_invitation_email(invitation.id)
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.email_sent);
        assert_eq!(result.email_status, InvitationEmailStatus::Sent);
        assert_eq!(result.email_message_id.as_deref(), Some("resend-email-id"));
        assert_eq!(
            email_sender.sent_to.lock().unwrap().as_slice(),
            &["invitee@example.com".to_string()]
        );
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.email_status, InvitationEmailStatus::Sent);
    }

    #[tokio::test]
    async fn resend_invitation_email_records_failed_email_status() {
        let (service, invitation_repo, _, _) = make_service(
            Err(EmailError::new("Resend failed\nwith details")),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();
        let invitation = invitation_repo
            .create(
                org.id.0,
                CreateInvitationRequest {
                    email: "invitee@example.com".to_string(),
                    role: MemberRole::Member,
                    expires_in_hours: 168,
                },
                org.owner_id.0,
            )
            .await
            .unwrap();

        let result = service
            .resend_invitation_email(invitation.id)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(!result.email_sent);
        assert_eq!(result.email_status, InvitationEmailStatus::Failed);
        assert_eq!(result.error.as_deref(), Some("Resend failed with details"));
        assert_eq!(
            result.email_last_error.as_deref(),
            Some("Resend failed with details")
        );
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.email_status, InvitationEmailStatus::Failed);
        assert_eq!(
            stored.email_last_error.as_deref(),
            Some("Resend failed with details")
        );
    }

    #[tokio::test]
    async fn resend_invitation_email_reports_missing_invitations_url() {
        let (service, invitation_repo, email_sender, _) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            None,
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();
        let invitation = invitation_repo
            .create(
                org.id.0,
                CreateInvitationRequest {
                    email: "invitee@example.com".to_string(),
                    role: MemberRole::Member,
                    expires_in_hours: 168,
                },
                org.owner_id.0,
            )
            .await
            .unwrap();

        let result = service
            .resend_invitation_email(invitation.id)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(!result.email_sent);
        assert_eq!(result.email_status, InvitationEmailStatus::Skipped);
        assert_eq!(
            result.error.as_deref(),
            Some("Invitation URL is not configured")
        );
        assert!(email_sender.sent_to.lock().unwrap().is_empty());
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.email_status, InvitationEmailStatus::Skipped);
    }

    #[tokio::test]
    async fn resend_invitation_email_rejects_non_pending_invitation() {
        let (service, invitation_repo, email_sender, _) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();
        let invitation = invitation_repo
            .create(
                org.id.0,
                CreateInvitationRequest {
                    email: "invitee@example.com".to_string(),
                    role: MemberRole::Member,
                    expires_in_hours: 168,
                },
                org.owner_id.0,
            )
            .await
            .unwrap();
        invitation_repo
            .update_status(invitation.id, InvitationStatus::Accepted)
            .await
            .unwrap();

        let error = service
            .resend_invitation_email(invitation.id)
            .await
            .unwrap_err();

        assert!(matches!(error, OrganizationError::InvalidParams(_)));
        assert!(email_sender.sent_to.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn resend_invitation_email_rejects_expired_invitation() {
        let (service, invitation_repo, email_sender, _) = make_service(
            Ok(EmailDeliveryOutcome::Sent {
                message_id: Some("resend-email-id".to_string()),
            }),
            Some("https://cloud.example.com/dashboard/invitations".to_string()),
        );
        let org = service
            .repository
            .get_by_id(Uuid::nil())
            .await
            .unwrap()
            .unwrap();
        let invitation = invitation_repo
            .create(
                org.id.0,
                CreateInvitationRequest {
                    email: "invitee@example.com".to_string(),
                    role: MemberRole::Member,
                    expires_in_hours: -1,
                },
                org.owner_id.0,
            )
            .await
            .unwrap();

        let error = service
            .resend_invitation_email(invitation.id)
            .await
            .unwrap_err();

        assert!(matches!(error, OrganizationError::InvalidParams(_)));
        assert!(email_sender.sent_to.lock().unwrap().is_empty());
        let stored = invitation_repo.records.lock().unwrap()[0].clone();
        assert_eq!(stored.status, InvitationStatus::Expired);
    }
}
