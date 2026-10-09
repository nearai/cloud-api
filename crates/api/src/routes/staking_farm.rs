use crate::middleware::{AdminUser, AuthenticatedUser};
use crate::models::ErrorResponse;
use crate::routes::admin::AdminAppState;
use crate::routes::api::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json as ResponseJson,
    Extension,
};
use serde::{Deserialize, Serialize};
use services::aml::AmlError;
use services::auth::UserId;
use services::organization::OrganizationId;
use services::staking_farm::{
    OrganizationStakingFarmSource, StakingFarmOrganizationInactive, StakingFarmSourceConflict,
};
use utoipa::ToSchema;
use uuid::Uuid;

type RouteResult<T> = Result<ResponseJson<T>, (StatusCode, ResponseJson<ErrorResponse>)>;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StakingFarmConfigResponse {
    pub enabled: bool,
    pub selected_org_binding_enabled: bool,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub sync_staleness_seconds: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StakingFarmStateResponse {
    pub organization_id: String,
    pub bound_at: String,
    pub bound_by_user_id: Option<String>,
    pub near_account_id: String,
    pub network_id: String,
    pub contract_id: String,
    pub farm_product_id: String,
    pub farm_price_id: Option<String>,
    pub credit_nano_usd_per_reward_unit: i64,
    pub accumulated_reward_units_24: Option<String>,
    pub pending_reward_units_24: Option<String>,
    pub total_earned_reward_units_24: Option<String>,
    pub farm_credit_nano_usd: Option<i64>,
    pub last_synced_reward_units_24: Option<String>,
    pub last_synced_credit_nano_usd: Option<i64>,
    pub last_synced_at: Option<String>,
    pub sync_status: String,
    pub last_sync_error: Option<String>,
    pub active_positions: serde_json::Value,
}

/// Get staking farm configuration
///
/// Returns the active House of Stake farm configuration used to convert
/// on-chain staking reward units into NEAR AI Cloud credits.
#[utoipa::path(
    get,
    path = "/v1/staking/farm/config",
    tag = "Staking Farm",
    responses(
        (status = 200, description = "Staking farm configuration", body = StakingFarmConfigResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_staking_farm_config(
    State(app_state): State<AppState>,
) -> RouteResult<StakingFarmConfigResponse> {
    let config = app_state.staking_farm_service.config();
    Ok(ResponseJson(StakingFarmConfigResponse {
        enabled: config.enabled,
        selected_org_binding_enabled: config.selected_org_binding_enabled,
        network_id: config.network_id.clone(),
        contract_id: config.contract_id.clone(),
        farm_product_id: config.farm_product_id.clone(),
        farm_price_id: config.farm_price_id.clone(),
        credit_nano_usd_per_reward_unit: config.credit_nano_usd_per_reward_unit,
        sync_staleness_seconds: config.sync_staleness_seconds,
    }))
}

/// Get organization staking farm state
///
/// Returns the staking farm source and last synced farm-credit state for an
/// organization, authorized by active membership independent of login provider.
#[utoipa::path(
    get,
    path = "/v1/organizations/{org_id}/staking/farm",
    tag = "Staking Farm",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization staking farm state", body = OrganizationStakingState),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "No staking farm source found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_organization_staking_farm(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> RouteResult<OrganizationStakingState> {
    let org = parse_uuid(&org_id, "Invalid organization ID")?;
    let role = require_org_role(&app_state, &user, org, false).await?;
    let source = app_state
        .staking_farm_service
        .get_source(org)
        .await
        .map_err(internal_error)?;
    let manage = matches!(
        role,
        services::organization::MemberRole::Owner | services::organization::MemberRole::Admin
    );
    let legacy = !app_state.config.staking_farm.selected_org_binding_enabled
        && require_near_default_org(&app_state, &user, org)
            .await
            .is_ok();
    let source = source.map(source_to_response);
    let eligible = match app_state.staking_farm_service.binding() {
        Some(binding) => binding.eligible(org).await.map_err(internal_error)?,
        None => false,
    };
    let mut legacy_fields = match source.as_ref() {
        Some(source) => serde_json::to_value(source)
            .map_err(internal_error)?
            .as_object()
            .cloned()
            .unwrap_or_default(),
        None => serde_json::Map::new(),
    };
    // The envelope already includes this field; avoid duplicate JSON keys.
    legacy_fields.remove("organization_id");
    Ok(ResponseJson(OrganizationStakingState {
        organization_id: org.to_string(),
        binding_status: if source.is_some() { "bound" } else { "unbound" }.into(),
        can_bind: eligible && manage && source.is_none(),
        can_sync: manage && (source.is_some() || legacy),
        can_manage_staking: manage && (source.is_some() || legacy),
        deletion_blocked: source.is_some(),
        eligible,
        legacy_fields,
        source,
    }))
}

/// Sync organization staking farm credits
///
/// Syncs the permanently bound source with owner/admin authorization. Legacy
/// default-organization source creation is available only while explicit binding is disabled.
#[utoipa::path(
    post,
    path = "/v1/organizations/{org_id}/staking/farm/sync",
    tag = "Staking Farm",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Synced organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 409, description = "NEAR account conflict or organization inactive", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn sync_organization_staking_farm(
    State(app_state): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    require_org_role(&app_state, &user, organization_id, true).await?;
    let existing = app_state
        .staking_farm_service
        .get_source(organization_id)
        .await
        .map_err(internal_error)?;
    let source = if let Some(source) = existing {
        app_state
            .staking_farm_service
            .sync_for_source(source, Some(user.0.id))
            .await
            .map_err(staking_farm_error)?
    } else if !app_state.config.staking_farm.selected_org_binding_enabled {
        require_near_default_org(&app_state, &user, organization_id).await?;
        app_state
            .staking_farm_service
            .sync_for_near_account(organization_id, user.0.provider_user_id.clone(), user.0.id)
            .await
            .map_err(staking_farm_error)?
    } else {
        return Err((
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "Bind a staking wallet first".into(),
                "staking_farm_not_bound".into(),
            )),
        ));
    };

    Ok(ResponseJson(source_to_response(source)))
}

/// Get admin organization staking farm state
///
/// Returns the staking farm source and last synced farm-credit state for any
/// organization. Requires platform admin access.
#[utoipa::path(
    get,
    path = "/v1/admin/organizations/{org_id}/staking/farm",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "No staking farm source found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn get_admin_organization_staking_farm(
    State(app_state): State<AdminAppState>,
    Extension(_admin_user): Extension<AdminUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    let source = app_state
        .staking_farm_service
        .get_source(organization_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found("No staking farm source found"))?;

    Ok(ResponseJson(source_to_response(source)))
}

/// Sync admin organization staking farm credits
///
/// Refreshes staking farm reward units and derived credits for any linked
/// organization. Requires platform admin access.
#[utoipa::path(
    post,
    path = "/v1/admin/organizations/{org_id}/staking/farm/sync",
    tag = "Admin",
    params(
        ("org_id" = String, Path, description = "Organization ID")
    ),
    responses(
        (status = 200, description = "Synced organization staking farm state", body = StakingFarmStateResponse),
        (status = 400, description = "Invalid organization ID", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 403, description = "Forbidden", body = ErrorResponse),
        (status = 404, description = "No staking farm source found", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub async fn sync_admin_organization_staking_farm(
    State(app_state): State<AdminAppState>,
    Extension(admin_user): Extension<AdminUser>,
    Path(org_id): Path<String>,
) -> RouteResult<StakingFarmStateResponse> {
    let organization_id = parse_uuid(&org_id, "Invalid organization ID")?;
    let source = app_state
        .staking_farm_service
        .get_source(organization_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| not_found("No staking farm source found"))?;
    let source = app_state
        .staking_farm_service
        .sync_for_source(source, Some(admin_user.0.id))
        .await
        .map_err(staking_farm_error)?;

    Ok(ResponseJson(source_to_response(source)))
}

async fn require_near_default_org(
    app_state: &AppState,
    user: &AuthenticatedUser,
    organization_id: Uuid,
) -> Result<(), (StatusCode, ResponseJson<ErrorResponse>)> {
    if user.0.auth_provider != "near" || user.0.provider_user_id.is_empty() {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Staking farm credits require NEAR wallet authentication".to_string(),
                "near_auth_required".to_string(),
            )),
        ));
    }

    // Use the earliest active membership, with the same ordering as /users/me.
    let orgs = app_state
        .organization_service
        .list_organizations_for_user(
            UserId(user.0.id),
            1,
            0,
            Some(services::organization::OrganizationOrderBy::JoinedAt),
            Some(services::organization::OrganizationOrderDirection::Asc),
        )
        .await
        .map_err(internal_error)?;

    let default_org = orgs
        .first()
        .ok_or_else(|| not_found("No default organization found for user"))?;
    if default_org.id != OrganizationId(organization_id) {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Staking farm credits can only be linked to the user's default organization"
                    .to_string(),
                "non_default_organization".to_string(),
            )),
        ));
    }

    Ok(())
}

fn source_to_response(source: OrganizationStakingFarmSource) -> StakingFarmStateResponse {
    StakingFarmStateResponse {
        organization_id: source.organization_id.to_string(),
        bound_at: source.created_at.to_rfc3339(),
        bound_by_user_id: source.created_by_user_id.map(|id| id.to_string()),
        near_account_id: source.near_account_id,
        network_id: source.network_id,
        contract_id: source.contract_id,
        farm_product_id: source.farm_product_id,
        farm_price_id: source.farm_price_id,
        credit_nano_usd_per_reward_unit: source.credit_nano_usd_per_reward_unit,
        accumulated_reward_units_24: source.last_synced_accumulated_reward_units_24,
        pending_reward_units_24: source.last_synced_pending_reward_units_24,
        total_earned_reward_units_24: source.last_synced_reward_units_24.clone(),
        farm_credit_nano_usd: source.last_synced_credit_nano_usd,
        last_synced_reward_units_24: source.last_synced_reward_units_24,
        last_synced_credit_nano_usd: source.last_synced_credit_nano_usd,
        last_synced_at: source.last_synced_at.map(|value| value.to_rfc3339()),
        sync_status: source.sync_status,
        last_sync_error: source.last_sync_error,
        active_positions: source.active_positions,
    }
}

fn parse_uuid(
    value: &str,
    message: &str,
) -> Result<Uuid, (StatusCode, ResponseJson<ErrorResponse>)> {
    Uuid::parse_str(value).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            ResponseJson(ErrorResponse::new(
                message.to_string(),
                "invalid_id".to_string(),
            )),
        )
    })
}

fn not_found(message: &str) -> (StatusCode, ResponseJson<ErrorResponse>) {
    (
        StatusCode::NOT_FOUND,
        ResponseJson(ErrorResponse::new(
            message.to_string(),
            "not_found".to_string(),
        )),
    )
}

fn staking_farm_error(error: anyhow::Error) -> (StatusCode, ResponseJson<ErrorResponse>) {
    if error
        .chain()
        .any(|cause| cause.downcast_ref::<StakingFarmSourceConflict>().is_some())
    {
        return (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "NEAR account is already linked to another organization".to_string(),
                "staking_farm_source_conflict".to_string(),
            )),
        );
    }

    if error.chain().any(|cause| {
        cause
            .downcast_ref::<StakingFarmOrganizationInactive>()
            .is_some()
    }) {
        return (
            StatusCode::CONFLICT,
            ResponseJson(ErrorResponse::new(
                "Organization is inactive and cannot be linked to a NEAR staking wallet"
                    .to_string(),
                "organization_inactive".to_string(),
            )),
        );
    }

    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<AmlError>(),
            Some(AmlError::AccountBlocked)
        )
    }) {
        return (
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Account error".to_string(),
                "account_error".to_string(),
            )),
        );
    }

    internal_error(error)
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, ResponseJson<ErrorResponse>) {
    tracing::error!(error = %error, "Staking farm route failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        ResponseJson(ErrorResponse::new(
            "Failed to process staking farm request".to_string(),
            "internal_server_error".to_string(),
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staking_farm_error_organization_inactive_returns_conflict() {
        let (status, body) = staking_farm_error(anyhow::anyhow!(StakingFarmOrganizationInactive));

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.0.error.r#type, "organization_inactive");
        assert!(body.0.error.message.contains("inactive"));
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct OrganizationStakingState {
    pub organization_id: String,
    pub binding_status: String,
    pub source: Option<StakingFarmStateResponse>,
    /// Keep bound response fields available to clients deployed before the capability envelope.
    #[serde(flatten)]
    pub legacy_fields: serde_json::Map<String, serde_json::Value>,
    pub eligible: bool,
    pub can_bind: bool,
    pub can_sync: bool,
    pub can_manage_staking: bool,
    pub deletion_blocked: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(tag = "phase", rename_all = "lowercase", deny_unknown_fields)]
pub enum BindRequest {
    Prepare {
        near_account_id: String,
    },
    Confirm {
        challenge_id: Uuid,
        signed_message: super::auth::NearSignedMessageJson,
        acknowledged_terms_version: String,
        idempotency_key: Uuid,
    },
}
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
pub enum BindResponse {
    Prepare(Box<services::staking_farm::binding::BindingChallenge>),
    Confirm {
        phase: String,
        binding_status: String,
        source: Box<StakingFarmStateResponse>,
        wallet_membership: services::staking_farm::binding::WalletMembership,
    },
}

async fn require_org_role(
    app: &AppState,
    user: &AuthenticatedUser,
    org: Uuid,
    manage: bool,
) -> Result<services::organization::MemberRole, (StatusCode, ResponseJson<ErrorResponse>)> {
    let role = app
        .organization_service
        .get_user_role(OrganizationId(org), UserId(user.0.id))
        .await
        .map_err(internal_error)?;
    let role = role.ok_or_else(|| {
        (
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Organization membership required".into(),
                "forbidden".into(),
            )),
        )
    })?;
    if manage && !role.can_manage_organization() {
        return Err((
            StatusCode::FORBIDDEN,
            ResponseJson(ErrorResponse::new(
                "Organization owner or admin permission required".into(),
                "forbidden".into(),
            )),
        ));
    }
    Ok(role)
}

#[utoipa::path(post, path = "/v1/organizations/{org_id}/staking/farm/bind", tag = "Staking Farm",
    params(("org_id" = String, Path)), request_body = BindRequest,
    responses((status = 200, body = BindResponse), (status = 400, body = ErrorResponse),
        (status = 403, body = ErrorResponse), (status = 409, body = ErrorResponse), (status = 429, body = ErrorResponse)),
    security(("session_token" = [])))]
pub async fn bind_organization_staking_farm(
    State(app): State<AppState>,
    Extension(user): Extension<AuthenticatedUser>,
    Path(org_id): Path<String>,
    ResponseJson(request): ResponseJson<BindRequest>,
) -> RouteResult<BindResponse> {
    let org = parse_uuid(&org_id, "Invalid organization ID")?;
    require_org_role(&app, &user, org, true).await?;
    let binding = app.staking_farm_service.binding().ok_or_else(|| {
        binding_error(services::staking_farm::binding::BindingError::Unavailable.into())
    })?;
    match request {
        BindRequest::Prepare { near_account_id } => {
            Ok(ResponseJson(BindResponse::Prepare(Box::new(
                binding
                    .prepare(org, user.0.id, near_account_id)
                    .await
                    .map_err(binding_error)?,
            ))))
        }
        BindRequest::Confirm {
            challenge_id,
            signed_message,
            acknowledged_terms_version,
            idempotency_key,
        } => {
            let proof = signed_message.try_into().map_err(|_| {
                binding_error(services::staking_farm::binding::BindingError::InvalidProof.into())
            })?;
            let wallet_membership = binding
                .confirm(
                    org,
                    user.0.id,
                    challenge_id,
                    idempotency_key,
                    &acknowledged_terms_version,
                    proof,
                )
                .await
                .map_err(binding_error)?;
            app.staking_farm_service.invalidate_source_cache(org).await;
            let source = app
                .staking_farm_service
                .get_source(org)
                .await
                .map_err(internal_error)?
                .ok_or_else(|| not_found("No staking source"))?;
            Ok(ResponseJson(BindResponse::Confirm {
                phase: "confirm".into(),
                binding_status: "bound".into(),
                source: Box::new(source_to_response(source)),
                wallet_membership,
            }))
        }
    }
}
fn binding_error(error: anyhow::Error) -> (StatusCode, ResponseJson<ErrorResponse>) {
    use services::staking_farm::binding::BindingError;
    let code = error.chain().find_map(|e| e.downcast_ref::<BindingError>());
    let (status, kind) = match code {
        Some(BindingError::Unavailable) => (StatusCode::CONFLICT, "staking_binding_unavailable"),
        Some(BindingError::Forbidden) => (StatusCode::FORBIDDEN, "forbidden"),
        Some(BindingError::Conflict) => (StatusCode::CONFLICT, "staking_farm_source_conflict"),
        Some(BindingError::RateLimited) => (StatusCode::TOO_MANY_REQUESTS, "binding_rate_limited"),
        Some(BindingError::AccountUnavailable) => (StatusCode::FORBIDDEN, "account_error"),
        Some(_) => (StatusCode::BAD_REQUEST, "invalid_binding_proof"),
        None => {
            if matches!(
                error.downcast_ref::<services::auth::near::NearAuthError>(),
                Some(services::auth::near::NearAuthError::InvalidSignature)
            ) {
                return (
                    StatusCode::BAD_REQUEST,
                    ResponseJson(ErrorResponse::new(
                        "Wallet proof is invalid".into(),
                        "invalid_binding_proof".into(),
                    )),
                );
            }
            if error
                .chain()
                .any(|e| matches!(e.downcast_ref::<AmlError>(), Some(AmlError::AccountBlocked)))
            {
                return (
                    StatusCode::FORBIDDEN,
                    ResponseJson(ErrorResponse::new(
                        "Account error".into(),
                        "account_error".into(),
                    )),
                );
            }
            // Do not log provider/database error details from wallet proof flows.
            tracing::error!("Wallet binding could not be completed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                ResponseJson(ErrorResponse::new(
                    "Unable to complete wallet binding".into(),
                    "internal_server_error".into(),
                )),
            );
        }
    };
    (
        status,
        ResponseJson(ErrorResponse::new(
            "Unable to complete wallet binding".into(),
            kind.into(),
        )),
    )
}

#[cfg(test)]
mod binding_request_tests {
    use super::*;
    #[test]
    fn binding_requires_explicit_phase_and_rejects_confirm_overrides() {
        assert!(serde_json::from_value::<BindRequest>(
            serde_json::json!({"phase":"prepare","near_account_id":"alice.near"})
        )
        .is_ok());
        for value in [
            serde_json::json!({"near_account_id":"alice.near"}),
            serde_json::json!({"phase":"other"}),
            serde_json::json!({"phase":"prepare","near_account_id":"alice.near","organization_id":"other"}),
            serde_json::json!({"phase":"confirm","near_account_id":"alice.near"}),
        ] {
            assert!(serde_json::from_value::<BindRequest>(value).is_err());
        }
    }
}
