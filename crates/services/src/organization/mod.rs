pub mod ports;
use super::auth::ports::{UserId, UserRepository};
use anyhow::Result;
use async_trait::async_trait;
pub use ports::*;
use std::sync::Arc;

pub struct OrganizationServiceImpl {
    repository: Arc<dyn OrganizationRepository>,
    user_repository: Arc<dyn UserRepository>,
    invitation_repository: Arc<dyn ports::OrganizationInvitationRepository>,
}

impl OrganizationServiceImpl {
    pub fn new(
        repository: Arc<dyn OrganizationRepository>,
        user_repository: Arc<dyn UserRepository>,
        invitation_repository: Arc<dyn ports::OrganizationInvitationRepository>,
    ) -> Self {
        Self {
            repository,
            user_repository,
            invitation_repository,
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

        let request = CreateOrganizationRequest {
            name,
            display_name: None,
            description,
        };

        self.repository
            .create(request, owner_id.0)
            .await
            .map_err(|e| {
                let error_msg = e.to_string();
                if error_msg.contains("duplicate key") || error_msg.contains("already exists") {
                    OrganizationError::AlreadyExists
                } else {
                    OrganizationError::InternalError(format!(
                        "Failed to create organization: {}",
                        e
                    ))
                }
            })
    }

    /// Get an organization by ID (private helper)
    async fn get_organization_impl(
        &self,
        id: OrganizationId,
    ) -> Result<Organization, OrganizationError> {
        self.repository
            .get_by_id(id.0)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get organization: {}", e))
            })?
            .ok_or(OrganizationError::NotFound)
    }

    /// Update an organization (private helper)
    async fn update_organization_impl(
        &self,
        id: OrganizationId,
        user_id: UserId,
        display_name: Option<String>,
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

        // Validate display_name if provided
        if let Some(ref n) = display_name {
            if n.trim().is_empty() {
                return Err(OrganizationError::InvalidParams(
                    "Organization display name cannot be empty".to_string(),
                ));
            }
        }

        let request = UpdateOrganizationRequest {
            display_name,
            description,
            rate_limit,
            settings,
        };

        self.repository.update(id.0, request).await.map_err(|e| {
            OrganizationError::InternalError(format!("Failed to update organization: {}", e))
        })
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

        self.repository.delete(id.0).await.map_err(|e| {
            OrganizationError::InternalError(format!("Failed to delete organization: {}", e))
        })
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
            .map_err(|e| {
                OrganizationError::InternalError(format!(
                    "Failed to list organizations for user: {}",
                    e
                ))
            })
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
            .map_err(|e| OrganizationError::InternalError(format!("Failed to add member: {}", e)))
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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to remove member: {}", e))
            })
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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to update member role: {}", e))
            })
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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to check membership: {}", e))
            })?;

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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get member role: {}", e))
            })?;

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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get member count: {}", e))
            })
    }

    /// Get organization by name (private helper)
    async fn get_organization_by_name_impl(
        &self,
        name: &str,
    ) -> Result<Option<Organization>, OrganizationError> {
        self.repository.get_by_name(name).await.map_err(|e| {
            OrganizationError::InternalError(format!("Failed to get organization by name: {}", e))
        })
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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to get members: {}", e))
            })?;

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
                    });
                }
                Err(e) => {
                    failed += 1;
                    results.push(InvitationResult {
                        email,
                        success: false,
                        member: None,
                        error: Some(format!("Failed to lookup user: {}", e)),
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
            .map_err(|e| OrganizationError::InternalError(format!("Failed to verify user: {}", e)))?
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
                    OrganizationError::InternalError(format!("Failed to check membership: {}", e))
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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to list members: {}", e))
            })?;

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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to remove member: {}", e))
            })
    }

    /// Create invitations for users (supports unregistered users, private helper)
    async fn create_invitations_impl(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        invitations: Vec<(String, MemberRole)>, // (email, role) pairs
        expires_in_hours: i64,
    ) -> Result<BatchInvitationResponse, OrganizationError> {
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
                Ok(_invitation) => {
                    successful += 1;
                    results.push(ports::InvitationResult {
                        email,
                        success: true,
                        member: None,
                        error: None,
                    });
                }
                Err(e) => {
                    failed += 1;
                    results.push(ports::InvitationResult {
                        email,
                        success: false,
                        member: None,
                        error: Some(format!("Failed to create invitation: {}", e)),
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
    ) -> Result<Vec<ports::OrganizationInvitation>, OrganizationError> {
        self.invitation_repository
            .list_by_email(email, Some(ports::InvitationStatus::Pending))
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to list invitations: {}", e))
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
                OrganizationError::InternalError(format!("Failed to get invitation: {}", e))
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
                OrganizationError::InternalError(format!("Failed to get invitation: {}", e))
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
                OrganizationError::InternalError(format!("Failed to get invitation: {}", e))
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
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to add member: {}", e))
            })?;

        // Mark invitation as accepted
        self.invitation_repository
            .update_status(invitation_id, ports::InvitationStatus::Accepted)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!("Failed to update invitation: {}", e))
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
                OrganizationError::InternalError(format!("Failed to get invitation: {}", e))
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
                OrganizationError::InternalError(format!("Failed to update invitation: {}", e))
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
                OrganizationError::InternalError(format!("Failed to list invitations: {}", e))
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
        display_name: Option<String>,
        description: Option<String>,
        rate_limit: Option<i32>,
        settings: Option<serde_json::Value>,
    ) -> Result<Organization, OrganizationError> {
        self.update_organization_impl(id, user_id, display_name, description, rate_limit, settings)
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

    async fn count_organizations_for_user(
        &self,
        user_id: UserId,
    ) -> Result<i64, OrganizationError> {
        self.repository
            .count_organizations_by_user(user_id.0)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!(
                    "Failed to count organizations for user: {}",
                    e
                ))
            })
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
    ) -> Result<Vec<OrganizationInvitation>, OrganizationError> {
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
}
