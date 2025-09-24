pub mod ports;
use super::auth::ports::UserId;
use anyhow::Result;
pub use ports::*;
use std::sync::Arc;

pub struct OrganizationService {
    repository: Arc<dyn OrganizationRepository>,
}

impl OrganizationService {
    pub fn new(repository: Arc<dyn OrganizationRepository>) -> Self {
        Self { repository }
    }

    /// Create a new organization
    pub async fn create_organization(
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
                OrganizationError::InternalError(format!("Failed to create organization: {}", e))
            })
    }

    /// Get an organization by ID
    pub async fn get_organization(
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

    /// Update an organization
    pub async fn update_organization(
        &self,
        id: OrganizationId,
        user_id: UserId,
        display_name: Option<String>,
        description: Option<String>,
        rate_limit: Option<i32>,
        settings: Option<serde_json::Value>,
    ) -> Result<Organization, OrganizationError> {
        // Check if user has permission
        let org = self.get_organization(id.clone()).await?;
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

    /// Delete an organization (owner only)
    pub async fn delete_organization(
        &self,
        id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if user is the owner
        let org = self.get_organization(id.clone()).await?;
        if org.owner_id != user_id {
            return Err(OrganizationError::Unauthorized(
                "Only the owner can delete an organization".to_string(),
            ));
        }

        self.repository.delete(id.0).await.map_err(|e| {
            OrganizationError::InternalError(format!("Failed to delete organization: {}", e))
        })
    }

    /// List organizations accessible to a user (where they are a member)
    pub async fn list_organizations_for_user(
        &self,
        user_id: UserId,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Organization>, OrganizationError> {
        self.repository
            .list_organizations_by_user(user_id.0, limit, offset)
            .await
            .map_err(|e| {
                OrganizationError::InternalError(format!(
                    "Failed to list organizations for user: {}",
                    e
                ))
            })
    }

    /// Add a member to an organization
    pub async fn add_member(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        new_member_id: UserId,
        role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization(organization_id.clone()).await?;
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

    /// Remove a member from an organization
    pub async fn remove_member(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization(organization_id.clone()).await?;

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

    /// Get all members of an organization
    pub async fn get_members(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<Vec<OrganizationMember>, OrganizationError> {
        // Check if requester is a member
        let org = self.get_organization(organization_id.clone()).await?;
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

        self.repository
            .list_members(organization_id.0)
            .await
            .map_err(|e| OrganizationError::InternalError(format!("Failed to get members: {}", e)))
    }

    /// Update a member's role
    pub async fn update_member_role(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
        member_id: UserId,
        new_role: MemberRole,
    ) -> Result<OrganizationMember, OrganizationError> {
        // Check if requester has permission
        let org = self.get_organization(organization_id.clone()).await?;

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

    /// Check if a user is a member of an organization
    pub async fn is_member(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<bool, OrganizationError> {
        // Check if user is owner
        let org = self.get_organization(organization_id.clone()).await?;
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

    /// Get a user's role in an organization
    pub async fn get_user_role(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<MemberRole>, OrganizationError> {
        // Check if user is owner
        let org = self.get_organization(organization_id.clone()).await?;
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

    /// Get the number of members in an organization
    pub async fn get_member_count(
        &self,
        organization_id: OrganizationId,
        requester_id: UserId,
    ) -> Result<i64, OrganizationError> {
        // Check if requester is a member
        let org = self.get_organization(organization_id.clone()).await?;
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

    /// Get organization by name
    pub async fn get_organization_by_name(
        &self,
        name: &str,
    ) -> Result<Option<Organization>, OrganizationError> {
        self.repository.get_by_name(name).await.map_err(|e| {
            OrganizationError::InternalError(format!("Failed to get organization by name: {}", e))
        })
    }
}
