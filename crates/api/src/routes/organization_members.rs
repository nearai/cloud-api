use crate::{
    conversions::{
        api_role_to_services_role, authenticated_user_to_user_id,
        services_invitation_result_to_api, services_member_to_api_member,
        services_member_with_user_to_api,
    },
    middleware::AuthenticatedUser,
    models::{ErrorResponse, ListOrganizationMembersResponse, PublicOrganizationMemberResponse},
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
    response::Json as ResponseJson,
};
use serde::Deserialize;
use services::organization::{OrganizationError, OrganizationId};
use tracing::{debug, error, warn};
use uuid::Uuid;

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
) -> Result<Json<crate::models::OrganizationMemberResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Adding member to organization: {} by user: {}",
        org_id, user.0.id
    );

    // Convert user_id from String to Uuid
    let user_id_to_add = request.user_id.parse::<Uuid>().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Invalid user ID".to_string(),
                "bad_request".to_string(),
            )),
        )
    })?;

    let organization_id = OrganizationId(org_id);
    let requester_id = authenticated_user_to_user_id(user);
    let new_member_id = services::auth::UserId(user_id_to_add);
    let role = api_role_to_services_role(request.role);

    match app_state
        .organization_service
        .add_member_validated(organization_id, requester_id, new_member_id, role)
        .await
    {
        Ok(member) => {
            let response = services_member_to_api_member(member);
            Ok(Json(response))
        }
        Err(OrganizationError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(OrganizationError::InvalidParams(msg)) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        )),
        Err(OrganizationError::AlreadyMember) => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "Already a member".to_string(),
                "conflict".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to add organization member: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to add organization member".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Invite users to an organization by email
///
/// Invites multiple users to the organization by their email addresses. The authenticated user must be an owner or admin.
/// Returns results for each invitation attempt, including successes and failures.
#[utoipa::path(
    post,
    path = "/organizations/{org_id}/members/invite-by-email",
    tag = "Organization Members",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = crate::models::InviteOrganizationMemberByEmailRequest,
    responses(
        (status = 200, description = "Invitation results (may include partial failures)", body = crate::models::InviteOrganizationMemberByEmailResponse),
        (status = 400, description = "Bad request - empty invitation list", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not an admin or owner", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn invite_organization_member_by_email(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<crate::models::InviteOrganizationMemberByEmailRequest>,
) -> Result<
    Json<crate::models::InviteOrganizationMemberByEmailResponse>,
    (StatusCode, Json<ErrorResponse>),
> {
    debug!(
        "Inviting {} members by email to organization: {} by user: {}",
        request.invitations.len(),
        org_id,
        user.0.id
    );

    // Validate request
    if request.invitations.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Empty invitations".to_string(),
                "bad_request".to_string(),
            )),
        ));
    }

    let organization_id = OrganizationId(org_id);
    let requester_id = authenticated_user_to_user_id(user);

    // Convert API invitations to service format (email, role pairs)
    let invitations: Vec<(String, services::organization::MemberRole)> = request
        .invitations
        .into_iter()
        .map(|inv| (inv.email, api_role_to_services_role(inv.role)))
        .collect();

    // Create invitations (supports unregistered users)
    const DEFAULT_EXPIRATION_HOURS: i64 = 168; // 7 days
    match app_state
        .organization_service
        .create_invitations(
            organization_id,
            requester_id,
            invitations,
            DEFAULT_EXPIRATION_HOURS,
        )
        .await
    {
        Ok(batch_response) => {
            debug!(
                "Invitation results: {} total, {} successful, {} failed",
                batch_response.total, batch_response.successful, batch_response.failed
            );

            let results = batch_response
                .results
                .into_iter()
                .map(services_invitation_result_to_api)
                .collect();

            Ok(Json(
                crate::models::InviteOrganizationMemberByEmailResponse {
                    results,
                    total: batch_response.total,
                    successful: batch_response.successful,
                    failed: batch_response.failed,
                },
            ))
        }
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(_) => {
            error!("Failed to invite organization members");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to invite organization members".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
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
) -> Result<Json<crate::models::OrganizationMemberResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Updating member {} in organization: {} by user: {}",
        user_id, org_id, user.0.id
    );

    let organization_id = OrganizationId(org_id);
    let requester_id = authenticated_user_to_user_id(user);
    let member_id = services::auth::UserId(user_id);
    let new_role = api_role_to_services_role(request.role);

    match app_state
        .organization_service
        .update_member_role_validated(organization_id, requester_id, member_id, new_role)
        .await
    {
        Ok(member) => {
            let response = services_member_to_api_member(member);
            Ok(Json(response))
        }
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(OrganizationError::InvalidParams(msg)) => Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        )),
        Err(_) => {
            error!("Failed to update organization member");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to update organization member".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
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
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Removing member {} from organization: {} by user: {}",
        user_id, org_id, user.0.id
    );

    let organization_id = OrganizationId(org_id);
    let requester_id = authenticated_user_to_user_id(user);
    let member_id = services::auth::UserId(user_id);

    match app_state
        .organization_service
        .remove_member_validated(organization_id, requester_id, member_id)
        .await
    {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Organization member not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(OrganizationError::Unauthorized(msg)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(msg, "forbidden".to_string())),
        )),
        Err(OrganizationError::InvalidParams(msg)) => {
            error!("Cannot remove member: {}", msg);
            Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(msg, "bad_request".to_string())),
            ))
        }
        Err(_) => {
            error!("Failed to remove organization member");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to remove organization member".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Query parameters for listing organization members
#[derive(Debug, Deserialize)]
pub struct ListMembersParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
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
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Number of records to return (default: 100, max: 1000)"),
        ("offset" = Option<i64>, Query, description = "Offset for pagination (default: 0)")
    ),
    responses(
        (status = 200, description = "List of organization members with public user information", body = ListOrganizationMembersResponse),
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
    Query(params): Query<ListMembersParams>,
) -> Result<Json<ListOrganizationMembersResponse>, (StatusCode, ResponseJson<ErrorResponse>)> {
    let user_id = user.0.id;
    debug!(
        "Listing members for organization: {} for user: {} (limit: {}, offset: {})",
        org_id, user_id, params.limit, params.offset
    );

    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let organization_id = OrganizationId(org_id);
    let requester_id = authenticated_user_to_user_id(user);

    // Get total count from service
    let total = match app_state
        .organization_service
        .get_member_count(organization_id.clone(), requester_id.clone())
        .await
    {
        Ok(count) => count,
        Err(services::organization::OrganizationError::Unauthorized(msg)) => {
            return Err((
                StatusCode::FORBIDDEN,
                ResponseJson(ErrorResponse::new(msg, "forbidden".to_string())),
            ));
        }
        Err(services::organization::OrganizationError::NotFound) => {
            return Err((
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Organization not found".to_string(),
                    "not_found".to_string(),
                )),
            ));
        }
        Err(_) => {
            error!("Failed to count organization members");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to count organization members".to_string(),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    };

    match app_state
        .organization_service
        .get_members_with_users_paginated(
            organization_id,
            requester_id,
            params.limit,
            params.offset,
        )
        .await
    {
        Ok(members) => {
            let member_responses: Vec<PublicOrganizationMemberResponse> = members
                .into_iter()
                .map(services_member_with_user_to_api)
                .collect();

            debug!(
                "Returning {} members for organization {} with public access level (total: {})",
                member_responses.len(),
                org_id,
                total
            );

            Ok(Json(ListOrganizationMembersResponse {
                members: member_responses,
                total,
                limit: params.limit,
                offset: params.offset,
            }))
        }
        Err(OrganizationError::Unauthorized(msg)) => {
            warn!("User attempted to access organization members without membership");
            Err((
                StatusCode::FORBIDDEN,
                ResponseJson(ErrorResponse::new(msg, "forbidden".to_string())),
            ))
        }
        Err(OrganizationError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            ResponseJson(ErrorResponse::new(
                "Organization not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(_) => {
            error!("Failed to list organization members");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Failed to list organization members".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}
