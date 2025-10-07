use crate::{
    conversions::db_user_to_public_user,
    middleware::AuthenticatedUser,
    models::{ErrorResponse, MemberRole, PublicOrganizationMemberResponse},
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, State},
    http::StatusCode,
};
use database;
use services::organization::ports::MemberRole as ServicesMemberRole;
use services::organization::ports::OrganizationRepository;
use tracing::{debug, error, warn};
use uuid::Uuid;

/// DEPRECATED: Legacy response type that exposes too much sensitive data
#[deprecated(note = "Use PublicOrganizationMemberResponse instead")]
#[derive(Debug, serde::Serialize)]
pub struct OrganizationMemberWithUser {
    #[serde(flatten)]
    pub member: database::OrganizationMember,
    pub user: database::User,
}

/// Add a member to an organization
///
/// Adds a new member to the organization. The authenticated user must be an owner or admin.
#[utoipa::path(
    post,
    path = "/organizations/{org_id}/members",
    tag = "Organization Members",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = crate::models::AddOrganizationMemberRequest,
    responses(
        (status = 200, description = "Member added successfully", body = crate::models::OrganizationMemberResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not an admin or owner", body = ErrorResponse),
        (status = 404, description = "User not found", body = ErrorResponse),
        (status = 409, description = "User is already a member", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn add_organization_member(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<crate::models::AddOrganizationMemberRequest>,
) -> Result<Json<crate::models::OrganizationMemberResponse>, StatusCode> {
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

    // Convert user_id from String to Uuid
    let user_id_to_add = request
        .user_id
        .parse::<Uuid>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // Verify the user to be added exists
    if let Ok(None) = app_state.db.users.get_by_id(user_id_to_add).await {
        return Err(StatusCode::NOT_FOUND);
    }

    // Convert API request to database request
    let db_request = database::AddOrganizationMemberRequest {
        user_id: user_id_to_add,
        role: match request.role {
            crate::models::MemberRole::Owner => database::OrganizationRole::Owner,
            crate::models::MemberRole::Admin => database::OrganizationRole::Admin,
            crate::models::MemberRole::Member => database::OrganizationRole::Member,
        },
    };

    let services_request = crate::conversions::db_add_member_req_to_services(db_request);
    match app_state
        .db
        .organizations
        .add_member(org_id, services_request, user.0.id)
        .await
    {
        Ok(member) => {
            let response = crate::conversions::services_member_to_api_member(member);
            Ok(Json(response))
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
///
/// Updates a member's role in the organization. The authenticated user must be an owner or admin.
/// Only owners can promote members to owner role.
#[utoipa::path(
    put,
    path = "/organizations/{org_id}/members/{user_id}",
    tag = "Organization Members",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("user_id" = Uuid, Path, description = "User ID of the member to update")
    ),
    request_body = crate::models::UpdateOrganizationMemberRequest,
    responses(
        (status = 200, description = "Member updated successfully", body = crate::models::OrganizationMemberResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not an admin or owner", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn update_organization_member(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((org_id, user_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<crate::models::UpdateOrganizationMemberRequest>,
) -> Result<Json<crate::models::OrganizationMemberResponse>, StatusCode> {
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
            if matches!(request.role, crate::models::MemberRole::Owner)
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

    // Convert API request to database request
    let db_request = database::UpdateOrganizationMemberRequest {
        role: match request.role {
            crate::models::MemberRole::Owner => database::OrganizationRole::Owner,
            crate::models::MemberRole::Admin => database::OrganizationRole::Admin,
            crate::models::MemberRole::Member => database::OrganizationRole::Member,
        },
    };

    let services_request = crate::conversions::db_update_member_req_to_services(db_request);
    match app_state
        .db
        .organizations
        .update_member(org_id, user_id, services_request)
        .await
    {
        Ok(member) => {
            let response = crate::conversions::services_member_to_api_member(member);
            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to update organization member: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Remove a member from an organization
///
/// Removes a member from the organization. The authenticated user must be an owner or admin,
/// or the member can remove themselves. The last owner cannot be removed.
#[utoipa::path(
    delete,
    path = "/organizations/{org_id}/members/{user_id}",
    tag = "Organization Members",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("user_id" = Uuid, Path, description = "User ID of the member to remove")
    ),
    responses(
        (status = 204, description = "Member removed successfully"),
        (status = 400, description = "Bad request - cannot remove last owner", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not an admin or owner", body = ErrorResponse),
        (status = 404, description = "Member not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
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

/// List organization members with limited user information
///
/// Returns limited user information for privacy and security:
/// - All members: See only public user info (username, display name, avatar)
/// - Sensitive data (email, last login, etc.) is not exposed to any organization members
#[utoipa::path(
    get,
    path = "/organizations/{org_id}/members",
    tag = "Organization Members",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "List of organization members with public user information", body = Vec<crate::models::PublicOrganizationMemberResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not a member of the organization", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_organization_members(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<PublicOrganizationMemberResponse>>, StatusCode> {
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
        Ok(Some(_member)) => {
            // User is a member, proceed
        }
        Ok(None) => {
            warn!(
                "User {} attempted to access organization {} members without membership",
                user.0.id, org_id
            );
            return Err(StatusCode::FORBIDDEN);
        }
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Get organization members
    let members = match app_state.db.organizations.list_members(org_id).await {
        Ok(members) => members,
        Err(e) => {
            error!("Failed to list organization members: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    // Get user details for each member with limited public information only
    let mut member_responses = Vec::new();
    for member in members {
        if let Ok(Some(user_data)) = app_state.db.users.get_by_id(member.user_id.0).await {
            // Return only public user details (no email, last_login, etc.)
            let response = PublicOrganizationMemberResponse {
                id: format!("{}_{}", member.organization_id.0, member.user_id.0),
                organization_id: member.organization_id.0.to_string(),
                role: convert_services_role_to_api(&member.role),
                joined_at: member.joined_at,
                user: db_user_to_public_user(&user_data),
            };

            member_responses.push(response);
        }
    }

    debug!(
        "Returning {} members for organization {} with public access level",
        member_responses.len(),
        org_id,
    );

    Ok(Json(member_responses))
}

/// Helper function to convert services member role to API member role
fn convert_services_role_to_api(role: &ServicesMemberRole) -> MemberRole {
    match role {
        ServicesMemberRole::Owner => MemberRole::Owner,
        ServicesMemberRole::Admin => MemberRole::Admin,
        ServicesMemberRole::Member => MemberRole::Member,
    }
}
