use crate::middleware::AuthenticatedUser;
use crate::models::ErrorResponse;
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
        .map_err(|error| match error {
            services::organization::ports::OrganizationError::NotFound => (
                StatusCode::NOT_FOUND,
                ResponseJson(ErrorResponse::new(
                    "Organization not found".into(),
                    "not_found".into(),
                )),
            ),
            error => internal_error(error),
        })?;
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
