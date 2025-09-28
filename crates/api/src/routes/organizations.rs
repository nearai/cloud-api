use crate::models::{
    ApiKeyResponse, CreateApiKeyRequest, CreateOrganizationRequest, ErrorResponse,
    OrganizationResponse, UpdateOrganizationRequest,
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
use uuid::Uuid;

/// List organizations
///
/// Lists all organizations accessible to the authenticated user.
#[utoipa::path(
    get,
    path = "/organizations",
    tag = "Organizations",
    responses(
        (status = 200, description = "List of organizations", body = Vec<OrganizationResponse>),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
        ("api_key" = [])
    )
)]
pub async fn list_organizations(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Query(params): Query<ListParams>,
) -> Result<Json<Vec<OrganizationResponse>>, StatusCode> {
    debug!("Listing organizations for user: {}", user.0.id);

    let user_id = crate::conversions::authenticated_user_to_user_id(user);

    match app_state
        .organization_service
        .list_organizations_for_user(user_id, params.limit, params.offset)
        .await
    {
        Ok(organizations) => {
            debug!("Found {} organizations for user", organizations.len());
            let response: Vec<OrganizationResponse> = organizations
                .into_iter()
                .map(crate::conversions::services_org_to_api_org)
                .collect();
            Ok(Json(response))
        }
        Err(e) => {
            error!("Failed to list organizations for user: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Query parameters for listing
#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    20
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
        ("bearer" = []),
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

            // Create a default workspace for the organization
            let workspace_repo = std::sync::Arc::new(
                database::repositories::WorkspaceRepository::new(app_state.db.pool().clone()),
            );
            let default_workspace_request = database::CreateWorkspaceRequest {
                name: "default".to_string(),
                display_name: "Default Workspace".to_string(),
                description: Some(format!("Default workspace for {}", request.name)),
            };

            match workspace_repo
                .create(default_workspace_request, org.id.0, user_id.0)
                .await
            {
                Ok(workspace) => {
                    debug!(
                        "Created default workspace: {} for organization: {}",
                        workspace.id, org.id.0
                    );
                }
                Err(e) => {
                    // Log the error but don't fail the organization creation
                    error!(
                        "Failed to create default workspace for organization {}: {}",
                        org.id.0, e
                    );
                }
            }

            Ok(Json(crate::conversions::services_org_to_api_org(org)))
        }
        Err(OrganizationError::InvalidParams(msg)) => {
            debug!("Invalid organization creation params: {}", msg);
            Err(StatusCode::BAD_REQUEST)
        }
        Err(e) => {
            error!("Failed to create organization: {}", e);
            if e.to_string().contains("duplicate key") || e.to_string().contains("already exists") {
                Err(StatusCode::CONFLICT)
            } else {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
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
        ("bearer" = []),
        ("api_key" = [])
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
                Err(e) => {
                    error!("Failed to get organization: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        }
        Ok(false) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to check organization membership: {}", e);
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
        ("bearer" = []),
        ("api_key" = [])
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
        Err(e) => {
            error!("Failed to update organization: {}", e);
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
        ("bearer" = []),
        ("api_key" = [])
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
            error!("Failed to delete organization {}", org_id);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(OrganizationError::NotFound) => Err(StatusCode::NOT_FOUND),
        Err(OrganizationError::Unauthorized(_)) => Err(StatusCode::FORBIDDEN),
        Err(e) => {
            error!("Failed to delete organization: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Create API key for organization
///
/// DEPRECATED: This endpoint is deprecated. Use workspace-based API key creation instead.
/// Creates a new API key for an organization.
#[deprecated(note = "Use POST /workspaces/{workspace_id}/api-keys instead")]
#[utoipa::path(
    post,
    path = "/organizations/{org_id}/api-keys",
    tag = "Organizations",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = CreateApiKeyRequest,
    responses(
        (status = 410, description = "Gone - This endpoint is deprecated. Use workspace-based API key creation.", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
    )
)]
pub async fn create_organization_api_key(
    _state: State<AppState>,
    _user: Extension<AuthenticatedUser>,
    _org_id: Path<Uuid>,
    _request: Json<CreateApiKeyRequest>,
) -> Result<Json<ApiKeyResponse>, StatusCode> {
    // This endpoint is deprecated in favor of workspace-based API keys
    error!("Attempted to use deprecated organization API key creation endpoint");
    Err(StatusCode::GONE) // HTTP 410 Gone - indicates the endpoint is deprecated
}

/// List API keys for organization
///
/// DEPRECATED: This endpoint is deprecated. Use workspace-based API key listing instead.
/// Returns a list of all API keys for an organization.
#[deprecated(note = "Use GET /workspaces/{workspace_id}/api-keys instead")]
#[utoipa::path(
    get,
    path = "/organizations/{org_id}/api-keys",
    tag = "Organizations",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 410, description = "Gone - This endpoint is deprecated. Use workspace-based API key listing.", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(
        ("bearer" = []),
        ("api_key" = [])
    )
)]
pub async fn list_organization_api_keys(
    _state: State<AppState>,
    _user: Extension<AuthenticatedUser>,
    _org_id: Path<Uuid>,
) -> Result<Json<Vec<ApiKeyResponse>>, StatusCode> {
    // This endpoint is deprecated in favor of workspace-based API keys
    error!("Attempted to use deprecated organization API key listing endpoint");
    Err(StatusCode::GONE) // HTTP 410 Gone - indicates the endpoint is deprecated
}
