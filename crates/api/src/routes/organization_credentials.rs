use crate::middleware::auth::{AuthState, AuthenticatedUser};
use crate::middleware::organization_credentials::{authenticate, error, ApiError};
use axum::{
    extract::{Extension, Path},
    Json,
};
use axum::{
    middleware::from_fn_with_state,
    routing::{delete, post},
    Router,
};
use chrono::{DateTime, Utc};
use database::repositories::organization_credentials::{
    CredentialRequest, OrganizationCredentialRepository, OrganizationOperationError,
};
use serde_json::Value;
use uuid::Uuid;

pub fn routes(repository: OrganizationCredentialRepository, auth: AuthState) -> Router {
    Router::new()
        .route(
            "/organizations/{org_id}/credentials",
            post(create_credential).get(list_credentials),
        )
        .route(
            "/organizations/{org_id}/credentials/{id}",
            delete(revoke_credential).put(update_credential),
        )
        .route(
            "/organizations/{org_id}/credentials/{id}/rotate",
            post(rotate_credential),
        )
        .layer(Extension(repository))
        .layer(from_fn_with_state(auth, authenticate))
}

#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/credentials",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization")
    ),
    request_body = CredentialRequestSchema,
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn create_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(org): Path<Uuid>,
    Json(request): Json<CredentialRequest>,
) -> Result<Json<Value>, ApiError> {
    let user =
        user.ok_or_else(|| error(OrganizationOperationError("credential_refused").into()))?;

    repository
        .save(org, user.0 .0.id, None, &request)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    delete,
    path = "/v1/organizations/{org_id}/credentials/{id}",
    tag = "Credits",
    params(
        ("org_id" = Uuid, Path, description = "Organization"),
        ("id" = Uuid, Path, description = "Record ID")
    ),
    responses((status = 200, description = "Authorized credit operation", body = Value)),
    security(("session_token" = []))
)]
pub async fn revoke_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let user =
        user.ok_or_else(|| error(OrganizationOperationError("credential_refused").into()))?;

    repository
        .revoke(org, user.0 .0.id, id)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/credentials",
    tag = "Organization credentials",
    params(("org_id" = Uuid, Path, description = "Organization")),
    responses((status = 200, description = "Credential configurations, lifetime usage and audit; no secrets", body = Value)),
    security(("session_token" = []))
)]
pub async fn list_credentials(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path(org): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let user =
        user.ok_or_else(|| error(OrganizationOperationError("credential_refused").into()))?;

    repository
        .list(org, user.0 .0.id)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    put,
    path = "/v1/organizations/{org_id}/credentials/{id}",
    tag = "Organization credentials",
    params(("org_id" = Uuid, Path, description = "Organization"), ("id" = Uuid, Path, description = "Credential")),
    request_body = CredentialRequestSchema,
    responses((status = 200, description = "Replace configuration; lifetime usage is preserved", body = Value)),
    security(("session_token" = []))
)]
pub async fn update_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    Json(request): Json<CredentialRequest>,
) -> Result<Json<Value>, ApiError> {
    let user =
        user.ok_or_else(|| error(OrganizationOperationError("credential_refused").into()))?;

    repository
        .save(org, user.0 .0.id, Some(id), &request)
        .await
        .map(Json)
        .map_err(error)
}

#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/credentials/{id}/rotate",
    tag = "Organization credentials",
    params(("org_id" = Uuid, Path, description = "Organization"), ("id" = Uuid, Path, description = "Credential")),
    responses((status = 200, description = "New secret shown once; old secret immediately invalid; budget unchanged", body = Value)),
    security(("session_token" = []))
)]
pub async fn rotate_credential(
    Extension(repository): Extension<OrganizationCredentialRepository>,
    user: Option<Extension<AuthenticatedUser>>,
    Path((org, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let user =
        user.ok_or_else(|| error(OrganizationOperationError("credential_refused").into()))?;

    repository
        .rotate(org, user.0 .0.id, id)
        .await
        .map(Json)
        .map_err(error)
}

// Keep wire schemas explicit even though repository DTOs own validation.
#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct TransferConstraintsSchema {
    /// Integer nano-USD. Omitted/null means no additional ceiling; zero blocks sends.
    #[schema(minimum = 0)]
    max_amount_per_transfer: Option<i64>,
    /// Lifetime nano-USD budget. Rotation and configuration updates never reset usage.
    #[schema(minimum = 0)]
    max_total_amount: Option<i64>,
    /// Policy default: payment and staking_farm. Grant requires explicit policy opt-in.
    /// Credential omission adds no restriction; an empty list permits no sources.
    allowed_source_types: Option<Vec<String>>,
    /// Omitted/null permits any recipient; an empty list permits none. At most 1,000 IDs.
    allowed_recipient_ids: Option<Vec<Uuid>>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct CredentialConstraintsSchema {
    #[schema(rename = "credits:transfer")]
    transfer: Option<TransferConstraintsSchema>,
}

#[allow(dead_code)]
#[derive(utoipa::ToSchema)]
pub struct CredentialRequestSchema {
    /// Nonempty display name, at most 200 UTF-8 bytes.
    name: String,
    /// Explicit supported permissions: credits:read and/or credits:transfer.
    permissions: Vec<String>,
    constraints: Option<CredentialConstraintsSchema>,
    /// Future timestamp no more than 366 days away.
    expires_at: DateTime<Utc>,
}
