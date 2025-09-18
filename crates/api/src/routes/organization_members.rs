use crate::{middleware::AuthenticatedUser, routes::api::AppState};
use axum::{
    extract::{Extension, Json, Path, State},
    http::StatusCode,
};
use database::{AddOrganizationMemberRequest, OrganizationMember, UpdateOrganizationMemberRequest};
use serde::Serialize;
use services::organization::ports::OrganizationRepository;
use tracing::{debug, error};
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct OrganizationMemberWithUser {
    #[serde(flatten)]
    pub member: OrganizationMember,
    pub user: database::User,
}

/// Add a member to an organization
pub async fn add_organization_member(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<AddOrganizationMemberRequest>,
) -> Result<Json<OrganizationMember>, StatusCode> {
    debug!(
        "Adding member to organization: {} by user: {}",
        org_id, user.0.id
    );

    // Check if user has permission to add members (must be owner or admin)
    match app_state
        .db
        .organizations
        .get_member(org_id, user.0.id)
        .await
    {
        Ok(Some(member)) => {
            if !member.role.can_manage_members() {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Verify the user to be added exists
    if let Ok(None) = app_state.db.users.get_by_id(request.user_id).await {
        return Err(StatusCode::NOT_FOUND);
    }

    let services_request = crate::conversions::db_add_member_req_to_services(request);
    match app_state
        .db
        .organizations
        .add_member(org_id, services_request, user.0.id)
        .await
    {
        Ok(member) => {
            let db_member = crate::conversions::services_member_to_db_member(member);
            Ok(Json(db_member))
        }
        Err(e) => {
            if e.to_string().contains("already a member") {
                Err(StatusCode::CONFLICT)
            } else {
                error!("Failed to add organization member: {}", e);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

/// Update an organization member's role
pub async fn update_organization_member(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<UpdateOrganizationMemberRequest>,
) -> Result<Json<OrganizationMember>, StatusCode> {
    debug!(
        "Updating member {} in organization: {} by user: {}",
        user_id, org_id, user.0.id
    );

    // Check if user has permission to update members
    match app_state
        .db
        .organizations
        .get_member(org_id, user.0.id)
        .await
    {
        Ok(Some(member)) => {
            if !member.role.can_manage_members() {
                return Err(StatusCode::FORBIDDEN);
            }

            // Prevent non-owners from promoting to owner
            if matches!(request.role, database::OrganizationRole::Owner)
                && !matches!(
                    member.role,
                    services::organization::ports::MemberRole::Owner
                )
            {
                return Err(StatusCode::FORBIDDEN);
            }
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    let services_request = crate::conversions::db_update_member_req_to_services(request);
    match app_state
        .db
        .organizations
        .update_member(org_id, user_id, services_request)
        .await
    {
        Ok(member) => {
            let db_member = crate::conversions::services_member_to_db_member(member);
            Ok(Json(db_member))
        }
        Err(e) => {
            error!("Failed to update organization member: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Remove a member from an organization
pub async fn remove_organization_member(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, StatusCode> {
    debug!(
        "Removing member {} from organization: {} by user: {}",
        user_id, org_id, user.0.id
    );

    // Check if user has permission to remove members
    match app_state
        .db
        .organizations
        .get_member(org_id, user.0.id)
        .await
    {
        Ok(Some(member)) => {
            if !member.role.can_manage_members() {
                // Allow members to remove themselves (leave organization)
                if user.0.id != user_id {
                    return Err(StatusCode::FORBIDDEN);
                }
            }
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Prevent removing the last owner
    if let Ok(members) = app_state.db.organizations.list_members(org_id).await {
        let owner_count = members
            .iter()
            .filter(|m| matches!(m.role, services::organization::ports::MemberRole::Owner))
            .count();

        if owner_count == 1 {
            if let Some(member) = members
                .iter()
                .find(|m| m.user_id == services::auth::ports::UserId(user_id))
            {
                if matches!(
                    member.role,
                    services::organization::ports::MemberRole::Owner
                ) {
                    error!("Cannot remove the last owner from organization");
                    return Err(StatusCode::BAD_REQUEST);
                }
            }
        }
    }

    match app_state
        .db
        .organizations
        .remove_member(org_id, user_id)
        .await
    {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to remove organization member: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// List organization members
pub async fn list_organization_members(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<OrganizationMemberWithUser>>, StatusCode> {
    debug!(
        "Listing members for organization: {} for user: {}",
        org_id, user.0.id
    );

    // Check if user has access to this organization
    match app_state
        .db
        .organizations
        .get_member(org_id, user.0.id)
        .await
    {
        Ok(Some(_)) => {
            // User is a member, allow access
        }
        Ok(None) => return Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Get organization members
    let members = match app_state.db.organizations.list_members(org_id).await {
        Ok(members) => members,
        Err(e) => {
            error!("Failed to list organization members: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Get user details for each member
    let mut members_with_users = Vec::new();
    for member in members {
        if let Ok(Some(user_data)) = app_state.db.users.get_by_id(member.user_id.0).await {
            members_with_users.push(OrganizationMemberWithUser {
                member: crate::conversions::services_member_to_db_member(member),
                user: user_data,
            });
        }
    }

    Ok(Json(members_with_users))
}

/// Get current user's role in an organization
pub async fn get_my_organization_role(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<OrganizationMember>, StatusCode> {
    debug!(
        "Getting role for user: {} in organization: {}",
        user.0.id, org_id
    );

    match app_state
        .db
        .organizations
        .get_member(org_id, user.0.id)
        .await
    {
        Ok(Some(member)) => {
            let db_member = crate::conversions::services_member_to_db_member(member);
            Ok(Json(db_member))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            error!("Failed to get organization member: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
