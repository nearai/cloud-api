//! Admin provider inventory. All writes use the shared admin authorization middleware.
use crate::{middleware::AdminUser, models::ErrorResponse, routes::admin::AdminAppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use services::provider_registry::{
    ProviderDefinition, ProviderModel, ProviderModelApproval, RegisteredProvider,
};
use uuid::Uuid;

type ApiError = (StatusCode, Json<ErrorResponse>);

#[utoipa::path(put,path="/v1/admin/providers/{id}/inventory",tag="Admin",params(("id"=Uuid,Path)),request_body=services::provider_registry::ManualInventoryEntry,responses((status=204)),security(("session_token"=[])))]
pub async fn save_inventory(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(actor): Extension<AdminUser>,
    Json(entry): Json<services::provider_registry::ManualInventoryEntry>,
) -> Result<StatusCode, ApiError> {
    require_provider(&state, id).await?;
    entry
        .validate()
        .map_err(|e| error(StatusCode::BAD_REQUEST, &e.to_string()))?;
    state
        .provider_registry
        .save_inventory(id, &entry, actor.0.id)
        .await
        .map_err(internal)?;
    reload(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}
fn error(status: StatusCode, message: &str) -> ApiError {
    (
        status,
        Json(ErrorResponse::new(
            message.into(),
            "provider_registry_error".into(),
        )),
    )
}
fn internal(_: impl std::fmt::Display) -> ApiError {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Provider registry operation failed",
    )
}

pub(crate) async fn reload(state: &AdminAppState) -> Result<(), ApiError> {
    state
        .inference_provider_pool
        .refresh_external_providers(state.provider_models_repository.as_ref())
        .await
        .map_err(internal)
}

#[utoipa::path(get,path="/v1/admin/providers",tag="Admin",responses((status=200,body=Vec<RegisteredProvider>)),security(("session_token"=[])))]
pub async fn list(
    State(state): State<AdminAppState>,
) -> Result<Json<Vec<RegisteredProvider>>, ApiError> {
    Ok(Json(
        state.provider_registry.list().await.map_err(internal)?,
    ))
}

#[utoipa::path(post,path="/v1/admin/providers",tag="Admin",request_body=ProviderDefinition,responses((status=201,body=RegisteredProvider)),security(("session_token"=[])))]
pub async fn create(
    State(state): State<AdminAppState>,
    Extension(actor): Extension<AdminUser>,
    Json(definition): Json<ProviderDefinition>,
) -> Result<(StatusCode, Json<RegisteredProvider>), ApiError> {
    save(&state, Uuid::new_v4(), definition, actor.0.id)
        .await
        .map(|p| (StatusCode::CREATED, Json(p)))
}

#[utoipa::path(put,path="/v1/admin/providers/{id}",tag="Admin",params(("id"=Uuid,Path)),request_body=ProviderDefinition,responses((status=200,body=RegisteredProvider)),security(("session_token"=[])))]
pub async fn update(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(actor): Extension<AdminUser>,
    Json(definition): Json<ProviderDefinition>,
) -> Result<Json<RegisteredProvider>, ApiError> {
    require_provider(&state, id).await?;
    Ok(Json(save(&state, id, definition, actor.0.id).await?))
}

async fn require_provider(state: &AdminAppState, id: Uuid) -> Result<RegisteredProvider, ApiError> {
    state
        .provider_registry
        .get(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Provider not found"))
}

async fn save(
    state: &AdminAppState,
    id: Uuid,
    definition: ProviderDefinition,
    actor: Uuid,
) -> Result<RegisteredProvider, ApiError> {
    definition
        .validate()
        .map_err(|e| error(StatusCode::BAD_REQUEST, &e.to_string()))?;
    state
        .provider_registry
        .save(id, &definition, actor)
        .await
        .map_err(internal)?;
    // Apply archive/configuration immediately, before potentially slow discovery.
    reload(state).await?;
    if state.provider_registry.sync(id).await.is_err() {
        tracing::warn!(provider_id=%id,"Provider saved; initial model discovery failed");
    }
    reload(state).await?;
    require_provider(state, id).await
}

#[utoipa::path(get,path="/v1/admin/providers/{id}/models",tag="Admin",params(("id"=Uuid,Path)),responses((status=200,body=Vec<ProviderModel>)),security(("session_token"=[])))]
pub async fn models(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<ProviderModel>>, ApiError> {
    require_provider(&state, id).await?;
    Ok(Json(
        state.provider_registry.models(id).await.map_err(internal)?,
    ))
}

#[utoipa::path(post,path="/v1/admin/providers/{id}/sync",tag="Admin",params(("id"=Uuid,Path)),responses((status=204)),security(("session_token"=[])))]
pub async fn sync(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_provider(&state, id).await?;
    state.provider_registry.sync(id).await.map_err(|_| {
        error(
            StatusCode::BAD_GATEWAY,
            "Discovery failed; previous inventory retained",
        )
    })?;
    reload(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(put,path="/v1/admin/providers/{id}/models",tag="Admin",params(("id"=Uuid,Path)),request_body=ProviderModelApproval,responses((status=204)),security(("session_token"=[])))]
pub async fn approve(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<ProviderModelApproval>,
) -> Result<StatusCode, ApiError> {
    require_provider(&state, id).await?;
    state.provider_registry.approve(id,&request,actor.0.id).await
        .map_err(|_| error(StatusCode::BAD_REQUEST,"Invalid model mapping: use an existing non-TEE external model and valid reasoning efforts"))?;
    reload(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelArchiveRequest {
    pub archived: bool,
    pub change_reason: Option<String>,
}

#[utoipa::path(put,path="/v1/admin/models/{model_name}/archive",tag="Admin",params(("model_name"=String,Path)),request_body=ModelArchiveRequest,responses((status=204)),security(("session_token"=[])))]
pub async fn archive_model(
    State(state): State<AdminAppState>,
    Path(name): Path<String>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<ModelArchiveRequest>,
) -> Result<StatusCode, ApiError> {
    if state
        .provider_models_repository
        .get_by_internal_name(&name)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(error(StatusCode::NOT_FOUND, "Model not found"));
    }
    if request.archived {
        return crate::routes::admin::delete_model(
            State(state),
            Path(name),
            Extension(actor),
            Some(Json(crate::models::DeleteModelRequest {
                change_reason: request.change_reason,
            })),
        )
        .await;
    }
    state
        .provider_models_repository
        .set_archived(
            &name,
            false,
            request.change_reason.clone(),
            Some(actor.0.id),
            Some(actor.0.email.clone()),
        )
        .await
        .map_err(internal)?;
    state.models_service.invalidate_models_cache().await;
    state
        .inference_provider_pool
        .invalidate_periodic_provider_refreshes()
        .await;
    let model = state
        .provider_models_repository
        .get_by_internal_name(&name)
        .await
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Model not found"))?;
    state
        .inference_provider_pool
        .refresh_pinned_provider_roles_from_catalog(
            &name,
            &model.provider_type,
            model.inference_url.is_some(),
        );
    if model.is_active && model.provider_type != "external" {
        if let Some(url) = &model.inference_url {
            let endpoints = services::inference_provider_pool::expand_inference_endpoints(
                &name,
                url,
                u32::try_from(model.context_length).ok(),
                model.provider_config.as_ref(),
            );
            state
                .inference_provider_pool
                .load_inference_url_models(endpoints, true)
                .await;
        }
    }
    reload(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicyUpdate {
    pub policy: services::provider_registry::ModelRoutingPolicy,
    pub change_reason: String,
}

#[utoipa::path(get,path="/v1/admin/models/{model_name}/routing-policy",tag="Admin",params(("model_name"=String,Path)),responses((status=200,body=services::provider_registry::ModelRoutingPolicy)),security(("session_token"=[])))]
pub async fn routing_policy(
    State(state): State<AdminAppState>,
    Path(name): Path<String>,
) -> Result<Json<services::provider_registry::ModelRoutingPolicy>, ApiError> {
    state
        .provider_registry
        .routing_policies()
        .await
        .map_err(internal)?
        .remove(&name)
        .map(Json)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Model not found"))
}

#[utoipa::path(put,path="/v1/admin/models/{model_name}/routing-policy",tag="Admin",params(("model_name"=String,Path)),request_body=RoutingPolicyUpdate,responses((status=204)),security(("session_token"=[])))]
pub async fn update_routing_policy(
    State(state): State<AdminAppState>,
    Path(name): Path<String>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<RoutingPolicyUpdate>,
) -> Result<StatusCode, ApiError> {
    if state
        .provider_models_repository
        .get_by_internal_name(&name)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(error(StatusCode::NOT_FOUND, "Model not found"));
    }
    state
        .provider_registry
        .save_routing_policy(&name, &request.policy, &request.change_reason, actor.0.id)
        .await
        .map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "Invalid routing policy or change reason",
            )
        })?;
    reload(&state).await?;
    state.models_service.invalidate_models_cache().await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccountMutation {
    pub account: services::provider_registry::entities::ProviderAccount,
    pub change_reason: String,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteMutation {
    pub route: services::provider_registry::entities::ModelRoute,
    pub change_reason: String,
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CostMutation {
    pub rates: services::provider_registry::entities::RouteCostRates,
    pub change_reason: String,
}

#[utoipa::path(get,path="/v1/admin/provider-accounts",tag="Admin",responses((status=200,body=Vec<services::provider_registry::entities::ProviderAccount>)),security(("session_token"=[])))]
pub async fn accounts(
    State(state): State<AdminAppState>,
) -> Result<Json<Vec<services::provider_registry::entities::ProviderAccount>>, ApiError> {
    Ok(Json(
        state.provider_registry.accounts().await.map_err(internal)?,
    ))
}

#[utoipa::path(put,path="/v1/admin/provider-accounts",tag="Admin",request_body=AccountMutation,responses((status=204)),security(("session_token"=[])))]
pub async fn save_account(
    State(state): State<AdminAppState>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<AccountMutation>,
) -> Result<StatusCode, ApiError> {
    state
        .provider_registry
        .save_account(&request.account, actor.0.id, &request.change_reason)
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid account or change reason"))?;
    reload(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get,path="/v1/admin/models/{model_name}/routes",tag="Admin",params(("model_name"=String,Path)),responses((status=200,body=Vec<services::provider_registry::entities::ModelRoute>)),security(("session_token"=[])))]
pub async fn model_routes(
    State(state): State<AdminAppState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<services::provider_registry::entities::ModelRoute>>, ApiError> {
    if state
        .provider_models_repository
        .get_by_internal_name(&name)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(error(StatusCode::NOT_FOUND, "Model not found"));
    }
    Ok(Json(
        state
            .provider_registry
            .model_routes(&name)
            .await
            .map_err(internal)?,
    ))
}

#[utoipa::path(put,path="/v1/admin/models/{model_name}/routes",tag="Admin",params(("model_name"=String,Path)),request_body=RouteMutation,responses((status=204)),security(("session_token"=[])))]
pub async fn save_route(
    State(state): State<AdminAppState>,
    Path(name): Path<String>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<RouteMutation>,
) -> Result<StatusCode, ApiError> {
    let model = state
        .provider_models_repository
        .get_by_internal_name(&name)
        .await
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Model not found"))?;
    if model.id != request.route.model_id {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Route model does not match path",
        ));
    }
    state
        .provider_registry
        .save_route(&request.route, actor.0.id, &request.change_reason)
        .await
        .map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "Invalid route, identity, security policy or change reason",
            )
        })?;
    reload(&state).await?;
    state.models_service.invalidate_models_cache().await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get,path="/v1/admin/provider-routes/{id}/cost-profiles",tag="Admin",params(("id"=Uuid,Path)),responses((status=200,body=Vec<serde_json::Value>)),security(("session_token"=[])))]
pub async fn cost_profiles(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    Ok(Json(
        state
            .provider_registry
            .cost_profiles(id)
            .await
            .map_err(internal)?,
    ))
}

#[utoipa::path(post,path="/v1/admin/provider-routes/{id}/cost-profiles",tag="Admin",params(("id"=Uuid,Path)),request_body=CostMutation,responses((status=201,body=Uuid)),security(("session_token"=[])))]
pub async fn save_cost_profile(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<CostMutation>,
) -> Result<(StatusCode, Json<Uuid>), ApiError> {
    let id = state
        .provider_registry
        .save_cost_profile(id, &request.rates, actor.0.id, &request.change_reason)
        .await
        .map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "Invalid route, cost rates or change reason",
            )
        })?;
    reload(&state).await?;
    Ok((StatusCode::CREATED, Json(id)))
}

#[utoipa::path(get,path="/v1/admin/provider-attempts/{request_id}",tag="Admin",params(("request_id"=Uuid,Path)),responses((status=200,body=Vec<serde_json::Value>)),security(("session_token"=[])))]
pub async fn attempts(
    State(state): State<AdminAppState>,
    Path(request_id): Path<Uuid>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    Ok(Json(
        state
            .provider_registry
            .attempts(request_id)
            .await
            .map_err(internal)?,
    ))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AttemptSettlement {
    pub actual_cost_microusd: i64,
    /// Non-secret invoice or reconciliation reference; never customer content.
    pub evidence: String,
}

#[utoipa::path(post,path="/v1/admin/provider-attempts/{id}/settlement",tag="Admin",params(("id"=Uuid,Path)),request_body=AttemptSettlement,responses((status=204)),security(("session_token"=[])))]
pub async fn settle_attempt(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<AttemptSettlement>,
) -> Result<StatusCode, ApiError> {
    state
        .provider_registry
        .settle_attempt(
            id,
            request.actual_cost_microusd,
            &request.evidence,
            actor.0.id,
        )
        .await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid or conflicting settlement"))?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post,path="/v1/admin/providers/import-legacy",tag="Admin",request_body=services::provider_registry::LegacyExternalImport,responses((status=200,body=serde_json::Value)),security(("session_token"=[])))]
pub async fn import_legacy(
    State(state): State<AdminAppState>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<services::provider_registry::LegacyExternalImport>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let report = state.provider_registry.import_external(&request, actor.0.id).await
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Import requires an unchanged legacy external configuration, provisioned credential reference, and valid phase"))?;
    if request.phase != "plan" {
        state.models_service.invalidate_models_cache().await;
        reload(&state).await?;
    }
    Ok(Json(report))
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OrganizationTeePolicyUpdate {
    pub tee_required: bool,
    pub change_reason: String,
}

#[utoipa::path(get,path="/v1/admin/organizations/{org_id}/inference-policy",tag="Admin",params(("org_id"=Uuid,Path)),responses((status=200,body=serde_json::Value)),security(("session_token"=[])))]
pub async fn organization_policy(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(
        serde_json::json!({"tee_required":state.provider_registry.organization_tee_policy(id).await.map_err(internal)?}),
    ))
}

#[utoipa::path(put,path="/v1/admin/organizations/{org_id}/inference-policy",tag="Admin",params(("org_id"=Uuid,Path)),request_body=OrganizationTeePolicyUpdate,responses((status=204)),security(("session_token"=[])))]
pub async fn save_organization_policy(
    State(state): State<AdminAppState>,
    Path(id): Path<Uuid>,
    Extension(actor): Extension<AdminUser>,
    Json(request): Json<OrganizationTeePolicyUpdate>,
) -> Result<StatusCode, ApiError> {
    state
        .provider_registry
        .save_organization_tee_policy(id, request.tee_required, actor.0.id, &request.change_reason)
        .await
        .map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "Invalid organization policy or change reason",
            )
        })?;
    Ok(StatusCode::NO_CONTENT)
}
