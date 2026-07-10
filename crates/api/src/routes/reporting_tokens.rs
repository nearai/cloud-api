use crate::{
    conversions::authenticated_user_to_user_id, middleware::AuthenticatedUser,
    models::ErrorResponse, routes::api::AppState,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use services::{
    common::RepositoryError,
    organization::{MemberRole, OrganizationError, OrganizationId},
    reporting_tokens::{CreateOrganizationReportingTokenRequest, OrganizationReportingToken},
};
use uuid::Uuid;

mod schemas;

pub use schemas::{
    CreateReportingTokenRequest, CreateReportingTokenResponse, ListReportingTokensResponse,
    ReportingTokenResponse,
};

type RouteError = (StatusCode, Json<ErrorResponse>);

/// Create a read-only reporting token.
///
/// Organization owners and admins can create reporting tokens for usage
/// export and summary endpoints. The raw `rpt-` token is returned once.
#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/reporting-tokens",
    tag = "Reporting",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    request_body = CreateReportingTokenRequest,
    responses(
        (status = 201, description = "Reporting token created", body = CreateReportingTokenResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn create_reporting_token(
    State(app_state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
    Json(request): Json<CreateReportingTokenRequest>,
) -> Result<(StatusCode, Json<CreateReportingTokenResponse>), RouteError> {
    request.validate().map_err(bad_request)?;
    require_reporting_token_manager(&app_state, user.clone(), org_id).await?;

    let user_id = authenticated_user_to_user_id(user).0;
    let created = app_state
        .reporting_token_repository
        .create(CreateOrganizationReportingTokenRequest {
            organization_id: org_id,
            name: request.name.trim().to_string(),
            created_by_user_id: user_id,
            expires_at: request.expires_at,
        })
        .await
        .map_err(map_repository_error)?;

    let token = created.token;
    Ok((
        StatusCode::CREATED,
        Json(CreateReportingTokenResponse {
            id: token.id,
            organization_id: token.organization_id,
            name: token.name,
            token: created.raw_token,
            token_prefix: token.token_prefix,
            created_by_user_id: token.created_by_user_id,
            created_at: token.created_at,
            expires_at: token.expires_at,
            last_used_at: token.last_used_at,
            scope: token.scope,
        }),
    ))
}

/// List active reporting tokens for an organization.
///
/// The response includes non-secret prefixes and audit timestamps. It never
/// includes raw reporting tokens or stored hashes.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/reporting-tokens",
    tag = "Reporting",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Active reporting tokens", body = ListReportingTokensResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn list_reporting_tokens(
    State(app_state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    Path(org_id): Path<Uuid>,
) -> Result<Json<ListReportingTokensResponse>, RouteError> {
    require_reporting_token_manager(&app_state, user, org_id).await?;

    let tokens = app_state
        .reporting_token_repository
        .list_active_by_organization(org_id)
        .await
        .map_err(map_repository_error)?;
    let reporting_tokens: Vec<ReportingTokenResponse> =
        tokens.into_iter().map(token_response).collect();
    let total = i64::try_from(reporting_tokens.len()).map_err(|_| internal_error())?;

    Ok(Json(ListReportingTokensResponse {
        reporting_tokens,
        total,
    }))
}

/// Revoke a reporting token.
///
/// Revoked reporting tokens can no longer authenticate usage reporting
/// requests.
#[utoipa::path(
    delete,
    path = "/v1/organizations/{org_id}/reporting-tokens/{token_id}",
    tag = "Reporting",
    params(
        ("org_id" = Uuid, Path, description = "Organization ID"),
        ("token_id" = Uuid, Path, description = "Reporting token ID")
    ),
    responses(
        (status = 204, description = "Reporting token revoked"),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "Reporting token not found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn revoke_reporting_token(
    State(app_state): State<AppState>,
    axum::Extension(user): axum::Extension<AuthenticatedUser>,
    Path((org_id, token_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, RouteError> {
    require_reporting_token_manager(&app_state, user.clone(), org_id).await?;

    let token = app_state
        .reporting_token_repository
        .get_by_id(token_id)
        .await
        .map_err(map_repository_error)?
        .filter(|token| token.organization_id == org_id)
        .ok_or_else(not_found)?;

    if token.revoked_at.is_some() {
        return Err(not_found());
    }

    let user_id = authenticated_user_to_user_id(user).0;
    match app_state
        .reporting_token_repository
        .revoke(token_id, user_id)
        .await
        .map_err(map_repository_error)?
    {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(not_found()),
    }
}

async fn require_reporting_token_manager(
    app_state: &AppState,
    user: AuthenticatedUser,
    org_id: Uuid,
) -> Result<(), RouteError> {
    let user_id = authenticated_user_to_user_id(user);
    let role = app_state
        .organization_service
        .get_user_role(OrganizationId(org_id), user_id)
        .await
        .map_err(map_organization_error)?;

    match role {
        Some(MemberRole::Owner | MemberRole::Admin) => Ok(()),
        Some(MemberRole::Member) | None => Err(forbidden()),
    }
}

fn token_response(token: OrganizationReportingToken) -> ReportingTokenResponse {
    ReportingTokenResponse {
        id: token.id,
        organization_id: token.organization_id,
        name: token.name,
        token_prefix: token.token_prefix,
        created_by_user_id: token.created_by_user_id,
        created_at: token.created_at,
        expires_at: token.expires_at,
        last_used_at: token.last_used_at,
        scope: token.scope,
    }
}

fn map_organization_error(error: OrganizationError) -> RouteError {
    match error {
        OrganizationError::NotFound => not_found(),
        OrganizationError::Unauthorized(_) => forbidden(),
        _ => internal_error(),
    }
}

fn map_repository_error(error: RepositoryError) -> RouteError {
    match error {
        RepositoryError::NotFound(_) => not_found(),
        RepositoryError::RequiredFieldMissing(message)
        | RepositoryError::ValidationFailed(message) => bad_request(message),
        _ => internal_error(),
    }
}

fn bad_request(message: String) -> RouteError {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new(message, "bad_request".to_string())),
    )
}

fn forbidden() -> RouteError {
    (
        StatusCode::FORBIDDEN,
        Json(ErrorResponse::new(
            "You are not authorized to manage reporting tokens for this organization.".to_string(),
            "forbidden".to_string(),
        )),
    )
}

fn not_found() -> RouteError {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse::new(
            "Reporting token not found".to_string(),
            "not_found".to_string(),
        )),
    )
}

fn internal_error() -> RouteError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse::new(
            "Failed to manage reporting tokens".to_string(),
            "internal_server_error".to_string(),
        )),
    )
}
