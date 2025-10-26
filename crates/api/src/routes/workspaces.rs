use crate::{
    conversions::authenticated_user_to_user_id,
    middleware::{auth::AuthenticatedApiKey, AuthenticatedUser},
    models::{
        ApiKeyResponse, CreateApiKeyRequest, ErrorResponse, ListApiKeysResponse,
        UpdateApiKeyRequest, UpdateApiKeySpendLimitRequest,
    },
    routes::api::AppState,
};
use axum::{
    extract::{Extension, Json, Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use services::organization::OrganizationId;
use tracing::{debug, error};
use utoipa::ToSchema;
use uuid::Uuid;
// ============================================
// Workspace Models
// ============================================

/// Request to create a new workspace
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateWorkspaceRequest {
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
}

/// Request to update a workspace
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateWorkspaceRequest {
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub settings: Option<serde_json::Value>,
}

/// Workspace response model
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WorkspaceResponse {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub organization_id: String,
    pub created_by_user_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub is_active: bool,
    pub settings: Option<serde_json::Value>,
}

/// Paginated workspaces list response
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ListWorkspacesResponse {
    pub workspaces: Vec<WorkspaceResponse>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Query parameters for listing
#[derive(Debug, Deserialize)]
pub struct ListParams {
    #[serde(default = "crate::routes::common::default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    pub order_by: Option<WorkspaceOrderBy>,
    pub order_direction: Option<WorkspaceOrderDirection>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOrderBy {
    CreatedAt,
}

impl From<WorkspaceOrderBy> for services::workspace::WorkspaceOrderBy {
    fn from(value: WorkspaceOrderBy) -> Self {
        match value {
            WorkspaceOrderBy::CreatedAt => Self::CreatedAt,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceOrderDirection {
    Asc,
    Desc,
}

impl From<WorkspaceOrderDirection> for services::workspace::WorkspaceOrderDirection {
    fn from(value: WorkspaceOrderDirection) -> Self {
        match value {
            WorkspaceOrderDirection::Asc => Self::Asc,
            WorkspaceOrderDirection::Desc => Self::Desc,
        }
    }
}

// ============================================
// Workspace Management Routes
// ============================================

/// Create a new workspace in an organization
///
/// Creates a new workspace within the specified organization. The authenticated user must
/// be a member of the organization to create workspaces.
#[utoipa::path(
    post,
    path = "/organizations/{org_id}/workspaces",
    tag = "Workspaces",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = CreateWorkspaceRequest,
    responses(
        (status = 201, description = "Workspace created successfully", body = WorkspaceResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not a member of organization", body = ErrorResponse),
        (status = 409, description = "Workspace name already exists in organization", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn create_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<(StatusCode, Json<WorkspaceResponse>), (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Creating workspace: {} in organization: {} by user: {}",
        request.name, org_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);
    let organization_id = OrganizationId(org_id);

    // Use the workspace service to create the workspace (it handles permission checking and duplicate detection)
    match app_state
        .workspace_service
        .create_workspace(
            request.name,
            request.display_name.unwrap_or_default(),
            request.description,
            organization_id,
            user_id,
        )
        .await
    {
        Ok(workspace) => {
            debug!(
                "Created workspace: {} in organization: {}",
                workspace.id.0, org_id
            );
            let response = WorkspaceResponse {
                id: workspace.id.0.to_string(),
                name: workspace.name,
                display_name: Some(workspace.display_name),
                description: workspace.description,
                organization_id: workspace.organization_id.0.to_string(),
                created_by_user_id: workspace.created_by_user_id.0.to_string(),
                created_at: workspace.created_at,
                updated_at: workspace.updated_at,
                is_active: workspace.is_active,
                settings: workspace.settings,
            };
            Ok((StatusCode::CREATED, Json(response)))
        }
        Err(services::workspace::WorkspaceError::Unauthorized(msg)) => {
            debug!("User is not authorized to create workspace: {}", msg);
            Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new(
                    "Workspace forbidden".to_string(),
                    "forbidden".to_string(),
                )),
            ))
        }
        Err(services::workspace::WorkspaceError::AlreadyExists) => {
            debug!("Workspace name already exists in organization");
            Err((
                StatusCode::CONFLICT,
                Json(ErrorResponse::new(
                    "Workspace name already exists in organization".to_string(),
                    "conflict".to_string(),
                )),
            ))
        }
        Err(services::workspace::WorkspaceError::InvalidParams(msg)) => {
            debug!("Invalid workspace parameters: {}", msg);
            Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    format!("Invalid workspace parameters: {}", msg),
                    "bad_request".to_string(),
                )),
            ))
        }
        Err(e) => {
            error!("Failed to create workspace: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to create workspace".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// List workspaces in an organization
///
/// Lists all workspaces within the specified organization. The authenticated user must
/// be a member of the organization to list workspaces.
#[utoipa::path(
    get,
    path = "/organizations/{org_id}/workspaces",
    tag = "Workspaces",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("limit" = Option<i64>, Query, description = "Maximum number of results"),
        ("offset" = Option<i64>, Query, description = "Number of results to skip"),
        ("order_by" = Option<WorkspaceOrderBy>, Query, description = "Order by"),
        ("order_direction" = Option<WorkspaceOrderDirection>, Query, description = "Order direction")
    ),
    responses(
        (status = 200, description = "Paginated list of workspaces", body = ListWorkspacesResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not a member of organization", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_organization_workspaces(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListWorkspacesResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Listing workspaces for organization: {} by user: {}",
        org_id, user.0.id
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let user_id = authenticated_user_to_user_id(user);
    let organization_id = OrganizationId(org_id);

    // Get total count from service
    let total = match app_state
        .workspace_service
        .count_workspaces_by_organization(organization_id.clone(), user_id.clone())
        .await
    {
        Ok(count) => count,
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new(
                    "Workspace forbidden".to_string(),
                    "forbidden".to_string(),
                )),
            ));
        }
        Err(e) => {
            error!("Failed to count workspaces: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list workspaces".to_string(),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    };

    // List workspaces with pagination using service
    match app_state
        .workspace_service
        .list_workspaces_for_organization_paginated(
            organization_id,
            user_id,
            params.limit,
            params.offset,
            params.order_by.map(From::from),
            params.order_direction.map(From::from),
        )
        .await
    {
        Ok(workspaces) => {
            let workspace_responses: Vec<WorkspaceResponse> = workspaces
                .into_iter()
                .map(|w| WorkspaceResponse {
                    id: w.id.0.to_string(),
                    name: w.name,
                    display_name: Some(w.display_name),
                    description: w.description,
                    organization_id: w.organization_id.0.to_string(),
                    created_by_user_id: w.created_by_user_id.0.to_string(),
                    created_at: w.created_at,
                    updated_at: w.updated_at,
                    is_active: w.is_active,
                    settings: w.settings,
                })
                .collect();

            Ok(Json(ListWorkspacesResponse {
                workspaces: workspace_responses,
                total,
                limit: params.limit,
                offset: params.offset,
            }))
        }
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Workspace forbidden".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to list workspaces: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list workspaces".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Get workspace by ID
///
/// Returns workspace details for a specific workspace ID.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "Workspace details", body = WorkspaceResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
) -> Result<Json<WorkspaceResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Getting workspace: {} by user: {}", workspace_id, user.0.id);

    let user_id = authenticated_user_to_user_id(user);
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);

    // Get workspace using service (includes permission checking)
    match app_state
        .workspace_service
        .get_workspace(workspace_id_typed, user_id)
        .await
    {
        Ok(workspace) => {
            let response = WorkspaceResponse {
                id: workspace.id.0.to_string(),
                name: workspace.name,
                display_name: Some(workspace.display_name),
                description: workspace.description,
                organization_id: workspace.organization_id.0.to_string(),
                created_by_user_id: workspace.created_by_user_id.0.to_string(),
                created_at: workspace.created_at,
                updated_at: workspace.updated_at,
                is_active: workspace.is_active,
                settings: workspace.settings,
            };
            Ok(Json(response))
        }
        Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Workspace forbidden".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to get workspace: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list workspaces".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Update workspace
///
/// Updates workspace details for a specific workspace ID.
#[utoipa::path(
    put,
    path = "/workspaces/{workspace_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    request_body = UpdateWorkspaceRequest,
    responses(
        (status = 200, description = "Updated workspace", body = WorkspaceResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn update_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
    Json(request): Json<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Updating workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);

    // Update workspace using service (includes permission checking)
    match app_state
        .workspace_service
        .update_workspace(
            workspace_id_typed,
            user_id,
            request.display_name,
            request.description,
            request.settings,
        )
        .await
    {
        Ok(updated) => {
            let response = WorkspaceResponse {
                id: updated.id.0.to_string(),
                name: updated.name,
                display_name: Some(updated.display_name),
                description: updated.description,
                organization_id: updated.organization_id.0.to_string(),
                created_by_user_id: updated.created_by_user_id.0.to_string(),
                created_at: updated.created_at,
                updated_at: updated.updated_at,
                is_active: updated.is_active,
                settings: updated.settings,
            };
            Ok(Json(response))
        }
        Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Workspace forbidden".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to update workspace: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to update workspace".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Delete workspace
///
/// Deletes (deactivates) a workspace. Only the workspace creator or organization admin/owner can delete.
#[utoipa::path(
    delete,
    path = "/workspaces/{workspace_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    responses(
        (status = 200, description = "Workspace deleted successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn delete_workspace(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Deleting workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user);
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);

    // Delete workspace using service (includes permission checking)
    match app_state
        .workspace_service
        .delete_workspace(workspace_id_typed, user_id)
        .await
    {
        Ok(true) => {
            debug!("Workspace {} deleted successfully", workspace_id);
            Ok(Json(serde_json::json!({
                "id": workspace_id.to_string(),
                "deleted": true
            })))
        }
        Ok(false) | Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Workspace forbidden".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to delete workspace: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to delete workspace".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

// ============================================
// Workspace API Key Management Routes
// ============================================

/// Create API key for workspace
///
/// Creates a new API key for a workspace.
#[utoipa::path(
    post,
    path = "/workspaces/{workspace_id}/api-keys",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID")
    ),
    request_body = CreateApiKeyRequest,
    responses(
        (status = 201, description = "API key created successfully", body = ApiKeyResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn create_workspace_api_key(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
    Json(request): Json<CreateApiKeyRequest>,
) -> Result<(StatusCode, Json<ApiKeyResponse>), (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Creating API key for workspace: {} by user: {}",
        workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user.clone());
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);
    let name = request.name.clone();

    // Check for duplicate API key name using service
    match app_state
        .workspace_service
        .check_api_key_name_duplication(workspace_id_typed.clone(), &name, user_id.clone())
        .await
    {
        Ok(true) => {
            return Err((
                StatusCode::CONFLICT,
                Json(ErrorResponse::new(
                    "API key with this name already exists in this workspace".to_string(),
                    "duplicate_api_key_name".to_string(),
                )),
            ));
        }
        Ok(false) => {
            // No duplicate, continue
        }
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new(
                    "Not authorized to create API key in this workspace".to_string(),
                    "forbidden".to_string(),
                )),
            ));
        }
        Err(e) => {
            error!("Failed to check API key name duplication: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to check for duplicate API key names".to_string(),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    };

    // Create API key request for services layer
    let services_request = crate::conversions::api_key_req_to_workspace_services(
        request,
        workspace_id_typed.clone(),
        user_id.clone(),
    );

    // Use the workspace service to create the API key
    match app_state
        .workspace_service
        .create_api_key(services_request)
        .await
    {
        Ok(api_key) => {
            debug!(
                "Created API key: {:?} for workspace: {}",
                api_key.id, workspace_id
            );
            let response = crate::conversions::workspace_api_key_to_api_response(api_key);
            Ok((StatusCode::CREATED, Json(response)))
        }
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Not authorized to create API key in this workspace".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to create API key: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to create API key".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// List API keys for workspace
///
/// Returns a paginated list of all API keys for a workspace with usage information.
#[utoipa::path(
    get,
    path = "/workspaces/{workspace_id}/api-keys",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID"),
        ("limit" = Option<i64>, Query, description = "Maximum number of results (default: 20)"),
        ("offset" = Option<i64>, Query, description = "Number of results to skip (default: 0)")
    ),
    responses(
        (status = 200, description = "Paginated list of workspace API keys", body = ListApiKeysResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_workspace_api_keys(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(workspace_id): Path<Uuid>,
    Query(params): Query<ListParams>,
) -> Result<Json<ListApiKeysResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Listing API keys for workspace: {} by user: {} (limit: {}, offset: {})",
        workspace_id, user.0.id, params.limit, params.offset
    );

    // Validate pagination parameters
    crate::routes::common::validate_limit_offset(params.limit, params.offset)?;

    let user_id = authenticated_user_to_user_id(user);
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);

    // Get total count from service
    let total = match app_state
        .workspace_service
        .count_api_keys_by_workspace(workspace_id_typed.clone(), user_id.clone())
        .await
    {
        Ok(count) => count,
        Err(services::workspace::WorkspaceError::NotFound) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new(
                    "Workspace not found".to_string(),
                    "not_found".to_string(),
                )),
            ));
        }
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => {
            return Err((
                StatusCode::FORBIDDEN,
                Json(ErrorResponse::new(
                    "Workspace forbidden".to_string(),
                    "forbidden".to_string(),
                )),
            ));
        }
        Err(e) => {
            error!("Failed to count API keys for workspace: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list API keys".to_string(),
                    "internal_server_error".to_string(),
                )),
            ));
        }
    };

    // Use workspace service to list workspace API keys with pagination and usage data
    match app_state
        .workspace_service
        .list_api_keys_paginated(workspace_id_typed, user_id, params.limit, params.offset)
        .await
    {
        Ok(api_keys) => {
            debug!(
                "Found {} API keys for workspace {}",
                api_keys.len(),
                workspace_id
            );
            let api_key_responses: Vec<ApiKeyResponse> = api_keys
                .into_iter()
                .map(crate::conversions::workspace_api_key_to_api_response)
                .collect();

            Ok(Json(ListApiKeysResponse {
                api_keys: api_key_responses,
                total,
                limit: params.limit,
                offset: params.offset,
            }))
        }
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Not authorized to list API keys in this workspace".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to list API keys: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to list API keys".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Revoke an API key
///
/// Revokes a specific API key from a workspace.
#[utoipa::path(
    delete,
    path = "/workspaces/{workspace_id}/api-keys/{key_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID"),
        ("key_id" = Uuid, Path, description = "API Key ID")
    ),
    responses(
        (status = 204, description = "API key revoked successfully"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "API key not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn revoke_workspace_api_key(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Revoking API key: {} in workspace: {} by user: {}",
        api_key_id, workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user.clone());
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);
    let api_key_id_typed = services::workspace::ApiKeyId(api_key_id.to_string());

    // Revoke API key using service (includes permission checking and validation)
    match app_state
        .workspace_service
        .revoke_api_key(workspace_id_typed, api_key_id_typed, user_id)
        .await
    {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) | Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::ApiKeyNotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "API key not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Not authorized to revoke API key in this workspace".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to revoke API key: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to revoke API key".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Revoke API key using workspace context from middleware
///
/// This route uses the workspace context from authenticated API keys to validate
/// and revoke keys. Used when the API key itself provides workspace context.
pub async fn revoke_api_key_with_context(
    State(app_state): State<AppState>,
    Extension(api_key_context): Extension<AuthenticatedApiKey>,
    Path(api_key_id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Revoking API key: {} with workspace context: {}",
        api_key_id, api_key_context.workspace.id.0
    );

    let workspace_id_typed = api_key_context.workspace.id.clone();
    let api_key_id_typed = services::workspace::ApiKeyId(api_key_id.to_string());
    let user_id = api_key_context.api_key.created_by_user_id.clone();

    // Revoke API key using service (includes permission checking and validation)
    match app_state
        .workspace_service
        .revoke_api_key(workspace_id_typed, api_key_id_typed, user_id)
        .await
    {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) | Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::ApiKeyNotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "API key not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Not authorized to revoke API key in this workspace".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to revoke API key: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to revoke API key".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Update API key spend limit
///
/// Updates the spending limit for a specific API key. The user must be a member of the
/// organization that owns the workspace. Set spend_limit to null to remove the limit.
#[utoipa::path(
    patch,
    path = "/workspaces/{workspace_id}/api-keys/{key_id}/spend-limit",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID"),
        ("key_id" = Uuid, Path, description = "API Key ID")
    ),
    request_body = UpdateApiKeySpendLimitRequest,
    responses(
        (status = 200, description = "Spend limit updated successfully", body = ApiKeyResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not authorized to update this key", body = ErrorResponse),
        (status = 404, description = "API key or workspace not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn update_api_key_spend_limit(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<UpdateApiKeySpendLimitRequest>,
) -> Result<Json<ApiKeyResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Updating spend limit for API key: {} in workspace: {} by user: {}",
        api_key_id, workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user.clone());
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);
    let api_key_id_typed = services::workspace::ApiKeyId(api_key_id.to_string());

    // Convert spend limit from API format to nano-dollars (scale 9)
    let spend_limit_nano = request.spend_limit.map(|limit| limit.amount);

    // Update the spend limit using service (includes permission checking and validation)
    match app_state
        .workspace_service
        .update_api_key_spend_limit(
            workspace_id_typed,
            api_key_id_typed,
            user_id,
            spend_limit_nano,
        )
        .await
    {
        Ok(updated_key) => {
            let response = crate::conversions::workspace_api_key_to_api_response(updated_key);
            Ok(Json(response))
        }
        Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::ApiKeyNotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "API key not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Not authorized to update API key spend limit".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to update API key spend limit: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to update spend limit".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}

/// Update API key
///
/// Updates an API key's name, expiration date, and/or spending limit. The user must be a member of the
/// organization that owns the workspace. All fields are optional - only provided fields will be updated.
#[utoipa::path(
    patch,
    path = "/workspaces/{workspace_id}/api-keys/{key_id}",
    tag = "Workspaces",
    params(
        ("workspace_id" = Uuid, Path, description = "Workspace ID"),
        ("key_id" = Uuid, Path, description = "API Key ID")
    ),
    request_body = UpdateApiKeyRequest,
    responses(
        (status = 200, description = "API key updated successfully", body = ApiKeyResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden - not authorized to update this key", body = ErrorResponse),
        (status = 404, description = "API key or workspace not found", body = ErrorResponse),
        (status = 409, description = "Conflict - API key with this name already exists", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = []),
    )
)]
pub async fn update_workspace_api_key(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path((workspace_id, api_key_id)): Path<(Uuid, Uuid)>,
    Json(request): Json<UpdateApiKeyRequest>,
) -> Result<Json<ApiKeyResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Updating API key: {} in workspace: {} by user: {}",
        api_key_id, workspace_id, user.0.id
    );

    let user_id = authenticated_user_to_user_id(user.clone());
    let workspace_id_typed = services::workspace::WorkspaceId(workspace_id);
    let api_key_id_typed = services::workspace::ApiKeyId(api_key_id.to_string());

    // Convert spend limit from API format to nano-dollars (scale 9)
    // If spend_limit is provided, wrap the amount in Some(Some(amount))
    let spend_limit_nano = request.spend_limit.map(|limit| Some(limit.amount));

    // Convert expires_at to Option<Option<DateTime<Utc>>>
    // If expires_at is provided, wrap it in Some(Some(value))
    let expires_at_opt = request.expires_at.map(Some);

    // Call the workspace service to update the API key
    match app_state
        .workspace_service
        .update_api_key(
            workspace_id_typed,
            api_key_id_typed,
            user_id,
            request.name,
            expires_at_opt,
            spend_limit_nano,
            request.is_active,
        )
        .await
    {
        Ok(updated_key) => {
            debug!("Updated API key: {:?}", updated_key.id);
            let response = crate::conversions::workspace_api_key_to_api_response(updated_key);
            Ok(Json(response))
        }
        Err(services::workspace::WorkspaceError::AlreadyExists) => Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse::new(
                "API key with this name already exists in this workspace".to_string(),
                "duplicate_api_key_name".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::Unauthorized(_)) => Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse::new(
                "Not authorized to update this API key".to_string(),
                "forbidden".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "Workspace not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(services::workspace::WorkspaceError::ApiKeyNotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                "API key not found".to_string(),
                "not_found".to_string(),
            )),
        )),
        Err(e) => {
            error!("Failed to update API key: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to update API key".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}
