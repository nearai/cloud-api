use crate::models::{
    CreateOrganizationRequest, ErrorResponse, ListOrganizationsResponse, OrganizationResponse,
    UpdateOrganizationRequest,
};
use crate::{middleware::AuthenticatedUser, routes::api::AppState};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use services::organization::{OrganizationError, OrganizationId};
use tracing::{debug, error};
use utoipa;
use utoipa::ToSchema;
use uuid::Uuid;

/// List organizations
///
/// Lists all organizations accessible to the authenticated user.
#[utoipa::path(
    get,
    path = "/organizations",
    tag = "Organizations",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number of organizations to return"),
        ("offset" = Option<i64>, Query, description = "Number of organizations to skip"),
        ("order_by" = Option<OrganizationOrderBy>, Query, description = "Order by"),
        ("order_direction" = Option<OrganizationOrderDirection>, Query, description = "Order direction")
    ),
    responses(
        (status = 200, description = "Paginated list of organizations", body = ListOrganizationsResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_organizations(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListOrganizationsParams>,
) -> Result<Json<ListOrganizationsResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Listing organizations for user: {}", user.0.id);

    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    // Get total count from service
    let total = match app_state
        .organization_service
        .count_organizations_for_user(user_id.clone())
        .await
    {
        Ok(count) => count,
        Err(_) => {
            error!("Failed to count organizations for user");
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to count organizations for user".to_string(),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    };

    match app_state
        .organization_service
        .list_organizations_for_user(
            user_id,
            params.limit,
            params.offset,
            params.order_by.map(From::from),
            params.order_direction.map(From::from),
        )
        .await
    {
        Ok(organizations) => {
            debug!("Found {} organizations for user", organizations.len());
            let org_responses: Vec<OrganizationResponse> = organizations
                .into_iter()
                .map(crate::conversions::services_org_to_api_org)
                .collect();

            Ok(Json(ListOrganizationsResponse {
                organizations: org_responses,
                total,
                limit: params.limit,
                offset: params.offset,
            }))
        }
        Err(_) => {
            error!("Failed to list organizations for user");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list organizations for user".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Query parameters for listing
#[derive(Debug, Deserialize)]
pub struct ListOrganizationsParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub order_by: Option<OrganizationOrderBy>,
    pub order_direction: Option<OrganizationOrderDirection>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationOrderBy {
    CreatedAt,
}

impl From<OrganizationOrderBy> for services::organization::OrganizationOrderBy {
    fn from(value: OrganizationOrderBy) -> Self {
        match value {
            OrganizationOrderBy::CreatedAt => Self::CreatedAt,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OrganizationOrderDirection {
    Asc,
    Desc,
}

impl From<OrganizationOrderDirection> for services::organization::OrganizationOrderDirection {
    fn from(value: OrganizationOrderDirection) -> Self {
        match value {
            OrganizationOrderDirection::Asc => Self::Asc,
            OrganizationOrderDirection::Desc => Self::Desc,
        }
    }
}

/// Create a new organization
///
/// Creates a new organization with the authenticated user as owner.
#[utoipa::path(
    post,
    path = "/organizations",
    tag = "Organizations",
    request_body = CreateOrganizationRequest,
    responses(
        (status = 200, description = "Organization created successfully", body = OrganizationResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 409, description = "Organization already exists", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn create_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Json(request): Json<CreateOrganizationRequest>,
) -> Result<Json<OrganizationResponse>, StatusCode> {
    debug!(
        "Creating organization: {} by user: {}",
        request.name, user.0.id
    );

    // Convert API request to services request
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .create_organization(request.name.clone(), request.description, user_id.clone())
        .await
    {
        Ok(org) => {
            debug!("Created organization: {} with owner: {}", org.id, user_id.0);

            // Create a default workspace for the organization using workspace service
            match app_state
                .workspace_service
                .create_workspace(
                    "default".to_string(),
                    "Default Workspace".to_string(),
                    Some(format!("Default workspace for {}", request.name)),
                    org.id.clone(),
                    user_id.clone(),
                )
                .await
            {
                Ok(workspace) => {
                    debug!(
                        "Created default workspace: {} for organization: {}",
                        workspace.id.0, org.id.0
                    );
                }
                Err(_) => {
                    // Log the error but don't fail the organization creation
                    error!("Failed to create default workspace for organization");
                }
            }

            Ok(Json(crate::conversions::services_org_to_api_org(org)))
        }
        Err(OrganizationError::InvalidParams(msg)) => {
            debug!("Invalid organization creation params: {}", msg);
            Err(StatusCode::BAD_REQUEST)
        }
        Err(OrganizationError::AlreadyExists) => {
            debug!("Organization already exists");
            Err(StatusCode::CONFLICT)
        }
        Err(_) => {
            error!("Failed to create organization");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Get organization by ID
///
/// Returns organization details for a specific organization ID.
#[utoipa::path(
    get,
    path = "/organizations/{org_id}",
    tag = "Organizations",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization details", body = OrganizationResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<OrganizationResponse>, StatusCode> {
    debug!("Getting organization: {} by user: {}", org_id, user.0.id);

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    // Check if user is a member or can access the organization
    match app_state
        .organization_service
        .is_member(organization_id.clone(), user_id)
        .await
    {
        Ok(true) => {
            // User is a member, get the organization
            match app_state
                .organization_service
                .get_organization(organization_id)
                .await
            {
                Ok(org) => Ok(Json(crate::conversions::services_org_to_api_org(org))),
                Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
                Err(_) => {
                    error!("Failed to get organization");
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(false) => Err(StatusCode::FORBIDDEN),
        Err(_) => {
            error!("Failed to check organization membership");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Update organization
///
/// Updates organization details for a specific organization ID.
#[utoipa::path(
    put,
    path = "/organizations/{org_id}",
    tag = "Organizations",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = UpdateOrganizationRequest,
    responses(
        (status = 200, description = "Updated organization", body = OrganizationResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn update_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<UpdateOrganizationRequest>,
) -> Result<Json<OrganizationResponse>, StatusCode> {
    debug!("Updating organization: {} by user: {}", org_id, user.0.id);

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .update_organization(
            organization_id,
            user_id,
            request.display_name,
            request.description,
            request.rate_limit,
            request.settings,
        )
        .await
    {
        Ok(updated_org) => Ok(Json(crate::conversions::services_org_to_api_org(
            updated_org,
        ))),
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(OrganizationError::InvalidParams(_)) => Err(StatusCode::BAD_REQUEST),
        Err(_) => {
            error!("Failed to update organization");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Delete organization (owner only)
///
/// Deletes an organization. Only the organization owner can perform this action.
#[utoipa::path(
    delete,
    path = "/organizations/{org_id}",
    tag = "Organizations",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization deleted successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Organization not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn delete_organization(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    debug!("Deleting organization: {} by user: {}", org_id, user.0.id);

    let organization_id = OrganizationId(org_id);
    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .delete_organization(organization_id, user_id)
        .await
    {
        Ok(true) => {
            debug!("Organization {} deleted successfully", org_id);
            Ok(Json(serde_json::json!({
                "id": org_id.to_string(),
                "deleted": true
            })))
        }
        Ok(false) => {
            error!("Failed to delete organization");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(_) => {
            error!("Failed to delete organization");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
