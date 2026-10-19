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
    let model = state
        .provider_models_repository
        .get_by_internal_name(&name)
        .await
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Model not found"))?;
    let mut batch = crate::models::BatchUpdateModelApiRequest::new();
    batch.insert(name,serde_json::from_value(serde_json::json!({"isActive":model.is_active,"changeReason":request.change_reason.unwrap_or_else(|| "Model unarchived".into())})).map_err(internal)?);
    let _ = crate::routes::admin::batch_upsert_models(State(state), Extension(actor), Json(batch))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
