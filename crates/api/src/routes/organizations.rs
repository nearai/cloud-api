use crate::models::{
    CreateOrganizationRequest, ErrorResponse, ListOrganizationsResponse, OrganizationResponse,
};
use crate::{middleware::AuthenticatedUser, routes::api::AppState};
use axum::{
    extract::{Extension, Json, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use services::organization::OrganizationError;
use tracing::{debug, error};
use utoipa;
use utoipa::ToSchema;

/// List organizations
///
/// Get all organizations you belong to.
#[utoipa::path(
    get,
    path = "/v1/organizations",
    tag = "Organizations",
    params(
        ("limit" = Option<i64>, Query, description = "Maximum number to return"),
        ("offset" = Option<i64>, Query, description = "Number to skip"),
        ("order_by" = Option<OrganizationOrderBy>, Query, description = "Sort by field"),
        ("order_direction" = Option<OrganizationOrderDirection>, Query, description = "Sort direction")
    ),
    responses(
        (status = 200, description = "List of organizations", body = ListOrganizationsResponse),
        (status = 401, description = "Invalid or missing session token", body = ErrorResponse),
        (status = 500, description = "Server error", body = ErrorResponse)
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
        .list_organizations_with_roles_for_user(
            user_id.clone(),
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
                .map(|org_with_role| {
                    crate::conversions::services_org_to_api_org(
                        org_with_role.organization,
                        crate::conversions::services_role_to_api_role(org_with_role.role),
                    )
                })
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
    JoinedAt,
}

impl From<OrganizationOrderBy> for services::organization::OrganizationOrderBy {
    fn from(value: OrganizationOrderBy) -> Self {
        match value {
            OrganizationOrderBy::CreatedAt => Self::CreatedAt,
            OrganizationOrderBy::JoinedAt => Self::JoinedAt,
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
    path = "/v1/organizations",
    tag = "Organizations",
    request_body = CreateOrganizationRequest,
    responses(
        (status = 200, description = "Organization created successfully", body = OrganizationResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 409, description = "Organization name is already taken", body = ErrorResponse),
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
) -> Result<Json<OrganizationResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(user_id = %user.0.id, "Creating organization");

    // Validate request to protect database from invalid or oversized data
    if let Err(msg) = request.validate() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(msg, "bad_request".to_string())),
        ));
    }

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

            Ok(Json(crate::conversions::services_org_to_api_org(
                org,
                crate::models::MemberRole::Owner,
            )))
        }
        Err(OrganizationError::InvalidParams(msg)) => {
            debug!("Invalid organization creation params: {}", msg);
            Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(msg, "bad_request".to_string())),
            ))
        }
        Err(OrganizationError::AlreadyExists) => {
            debug!("Organization already exists");
            Err(crate::routes::common::map_organization_error(
                OrganizationError::AlreadyExists,
            ))
        }
        Err(_) => {
            error!("Failed to create organization");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to create organization".to_string(),
                    "internal_server_error".to_string(),
                )),
            ))
        }
    }
}
