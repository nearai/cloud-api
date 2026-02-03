use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{
    models::{
        CreateVectorStoreFileBatchRequest, CreateVectorStoreFileRequest, CreateVectorStoreRequest,
        ErrorResponse, ModifyVectorStoreRequest, UpdateVectorStoreFileAttributesRequest,
        VectorStoreDeleteResponse, VectorStoreFileBatchObject, VectorStoreFileCounts,
        VectorStoreFileDeleteResponse, VectorStoreFileListResponse, VectorStoreFileObject,
        VectorStoreFileStatus, VectorStoreListResponse, VectorStoreObject,
    },
    routes::api::AppState,
};
use services::{
    id_prefixes::{PREFIX_FILE, PREFIX_VS, PREFIX_VSF, PREFIX_VSFB},
    vector_stores::*,
    workspace::ApiKey,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_uuid_with_prefix(
    id_str: &str,
    prefix: &str,
) -> Result<Uuid, (StatusCode, Json<ErrorResponse>)> {
    let raw = id_str.strip_prefix(prefix).unwrap_or(id_str);
    Uuid::parse_str(raw).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                format!("Invalid ID format: {id_str}"),
                "invalid_request_error".to_string(),
            )),
        )
    })
}

fn map_service_error(e: VectorStoreServiceError) -> (StatusCode, Json<ErrorResponse>) {
    let (status, error_type) = match &e {
        VectorStoreServiceError::NotFound
        | VectorStoreServiceError::FileNotFound
        | VectorStoreServiceError::BatchNotFound => (StatusCode::NOT_FOUND, "not_found"),
        VectorStoreServiceError::InvalidParams(_) => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
        VectorStoreServiceError::FileAlreadyExists => (StatusCode::CONFLICT, "conflict"),
        VectorStoreServiceError::FileNotAccessible(_) => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
        VectorStoreServiceError::RepositoryError(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    };

    if status.is_client_error() {
        tracing::warn!("Vector store operation failed: {}", e);
    } else {
        tracing::error!("Vector store operation failed: {}", e);
    }

    // Sanitize error messages for internal server errors to avoid leaking
    // database schema details or internal state to the client
    let message = if status.is_server_error() {
        "An internal error occurred".to_string()
    } else {
        e.to_string()
    };

    (
        status,
        Json(ErrorResponse::new(message, error_type.to_string())),
    )
}

fn to_vector_store_object(vs: &VectorStore) -> VectorStoreObject {
    let expires_after = vs.expires_after_anchor.as_ref().map(|anchor| {
        let anchor_enum = serde_json::from_value::<crate::models::ExpiresAfterAnchor>(
            Value::String(anchor.clone()),
        )
        .unwrap_or(crate::models::ExpiresAfterAnchor::LastActiveAt);
        crate::models::VectorStoreExpiresAfter {
            anchor: anchor_enum,
            days: vs.expires_after_days.unwrap_or(0),
        }
    });

    // Convert metadata from Value to HashMap
    let metadata: Option<HashMap<String, Value>> = match &vs.metadata {
        Value::Object(map) => {
            if map.is_empty() {
                None
            } else {
                Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            }
        }
        _ => None,
    };

    let status = serde_json::from_value::<crate::models::VectorStoreStatus>(Value::String(
        vs.status.clone(),
    ))
    .unwrap_or(crate::models::VectorStoreStatus::Completed);

    VectorStoreObject {
        id: format!("{PREFIX_VS}{}", vs.id),
        object: "vector_store".to_string(),
        created_at: vs.created_at.timestamp(),
        last_active_at: Some(vs.last_active_at.timestamp()),
        name: vs.name.clone(),
        description: vs.description.clone(),
        status,
        usage_bytes: vs.usage_bytes,
        file_counts: VectorStoreFileCounts {
            in_progress: vs.file_counts_in_progress,
            completed: vs.file_counts_completed,
            failed: vs.file_counts_failed,
            cancelled: vs.file_counts_cancelled,
            total: vs.file_counts_total,
        },
        expires_after,
        expires_at: vs.expires_at.map(|dt| dt.timestamp()),
        metadata,
    }
}

fn to_vector_store_file_object(vsf: &VectorStoreFile) -> VectorStoreFileObject {
    let chunking_strategy = vsf
        .chunking_strategy
        .as_ref()
        .and_then(|v| serde_json::from_value::<crate::models::ChunkingStrategy>(v.clone()).ok());

    let attributes: Option<HashMap<String, Value>> = match &vsf.attributes {
        Value::Object(map) => {
            if map.is_empty() {
                None
            } else {
                Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            }
        }
        _ => None,
    };

    let status = serde_json::from_value::<VectorStoreFileStatus>(Value::String(
        vsf.status.clone(),
    ))
    .unwrap_or(VectorStoreFileStatus::InProgress);

    VectorStoreFileObject {
        id: format!("{PREFIX_VSF}{}", vsf.id),
        object: "vector_store.file".to_string(),
        created_at: vsf.created_at.timestamp(),
        vector_store_id: format!("{PREFIX_VS}{}", vsf.vector_store_id),
        status,
        last_error: vsf.last_error.as_ref().and_then(|v| {
            serde_json::from_value::<crate::models::VectorStoreFileError>(v.clone()).ok()
        }),
        usage_bytes: vsf.usage_bytes,
        chunking_strategy,
        attributes,
    }
}

fn to_file_batch_object(batch: &VectorStoreFileBatch) -> VectorStoreFileBatchObject {
    let status = serde_json::from_value::<VectorStoreFileStatus>(Value::String(
        batch.status.clone(),
    ))
    .unwrap_or(VectorStoreFileStatus::InProgress);

    VectorStoreFileBatchObject {
        id: format!("{PREFIX_VSFB}{}", batch.id),
        object: "vector_store.file_batch".to_string(),
        created_at: batch.created_at.timestamp(),
        vector_store_id: format!("{PREFIX_VS}{}", batch.vector_store_id),
        status,
        file_counts: VectorStoreFileCounts {
            in_progress: batch.file_counts_in_progress,
            completed: batch.file_counts_completed,
            failed: batch.file_counts_failed,
            cancelled: batch.file_counts_cancelled,
            total: batch.file_counts_total,
        },
    }
}

// ---------------------------------------------------------------------------
// Query parameter structs
// ---------------------------------------------------------------------------

/// Query parameters for list endpoints
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ListQueryParams {
    /// A limit on the number of objects to be returned.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Sort order by the `created_at` timestamp.
    #[serde(default)]
    pub order: Option<String>,
    /// A cursor for use in pagination. `after` is an object ID that defines your place in the list.
    #[serde(default)]
    pub after: Option<String>,
    /// A cursor for use in pagination. `before` is an object ID that defines your place in the list.
    #[serde(default)]
    pub before: Option<String>,
}

/// Query parameters for listing files in a batch
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct BatchFileListQueryParams {
    /// A limit on the number of objects to be returned.
    #[serde(default)]
    pub limit: Option<i64>,
    /// Sort order by the `created_at` timestamp.
    #[serde(default)]
    pub order: Option<String>,
    /// A cursor for use in pagination.
    #[serde(default)]
    pub after: Option<String>,
    /// A cursor for use in pagination.
    #[serde(default)]
    pub before: Option<String>,
    /// Filter by file status.
    #[serde(default)]
    pub filter: Option<VectorStoreFileStatus>,
}

// ---------------------------------------------------------------------------
// Vector Store CRUD
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/vector_stores",
    tag = "Vector Stores",
    request_body = CreateVectorStoreRequest,
    responses(
        (status = 200, body = VectorStoreObject),
        (status = 400, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Json(body): Json<CreateVectorStoreRequest>,
) -> Result<Json<VectorStoreObject>, (StatusCode, Json<ErrorResponse>)> {
    let workspace_id = api_key.workspace_id.0;

    let chunking_strategy = body
        .chunking_strategy
        .as_ref()
        .and_then(|cs| serde_json::to_value(cs).ok());

    let metadata = body
        .metadata
        .as_ref()
        .map(|m| serde_json::to_value(m).unwrap_or_default());

    let expires_after_anchor = body.expires_after.as_ref().map(|ea| {
        serde_json::to_value(&ea.anchor)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "last_active_at".to_string())
    });
    let expires_after_days = body.expires_after.as_ref().map(|ea| ea.days);

    let mut vs = app_state
        .vector_store_service
        .create_vector_store(CreateVectorStoreParams {
            workspace_id,
            name: body.name,
            description: body.description,
            expires_after_anchor,
            expires_after_days,
            metadata,
            chunking_strategy: chunking_strategy.clone(),
        })
        .await
        .map_err(map_service_error)?;

    // If file_ids were provided, add each file to the vector store
    if let Some(file_ids) = &body.file_ids {
        for fid_str in file_ids {
            let file_uuid = parse_uuid_with_prefix(fid_str, PREFIX_FILE)?;
            let _ = app_state
                .vector_store_service
                .create_vector_store_file(CreateVectorStoreFileParams {
                    vector_store_id: vs.id,
                    file_id: file_uuid,
                    workspace_id,
                    batch_id: None,
                    chunking_strategy: chunking_strategy.clone(),
                    attributes: None,
                })
                .await
                .map_err(map_service_error)?;
        }

        // Re-fetch the store for updated file counts
        vs = app_state
            .vector_store_service
            .get_vector_store(vs.id, workspace_id)
            .await
            .map_err(map_service_error)?;
    }

    Ok(Json(to_vector_store_object(&vs)))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores",
    tag = "Vector Stores",
    params(ListQueryParams),
    responses(
        (status = 200, body = VectorStoreListResponse),
        (status = 400, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_stores(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    axum::extract::Query(params): axum::extract::Query<ListQueryParams>,
) -> Result<Json<VectorStoreListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let workspace_id = api_key.workspace_id.0;
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let order = params.order.unwrap_or_else(|| "desc".to_string());

    let after = params
        .after
        .as_deref()
        .map(|s| parse_uuid_with_prefix(s, PREFIX_VS))
        .transpose()?;

    let before = params
        .before
        .as_deref()
        .map(|s| parse_uuid_with_prefix(s, PREFIX_VS))
        .transpose()?;

    let stores = app_state
        .vector_store_service
        .list_vector_stores(&ListParams {
            workspace_id,
            limit: limit + 1,
            order,
            after,
            before,
            filter: None,
        })
        .await
        .map_err(map_service_error)?;

    let has_more = stores.len() > limit as usize;
    let stores_to_return: Vec<_> = stores.into_iter().take(limit as usize).collect();

    let data: Vec<VectorStoreObject> = stores_to_return
        .iter()
        .map(to_vector_store_object)
        .collect();
    let first_id = data.first().map(|o| o.id.clone());
    let last_id = data.last().map(|o| o.id.clone());

    Ok(Json(VectorStoreListResponse {
        object: "list".to_string(),
        data,
        first_id,
        last_id,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store to retrieve")
    ),
    responses(
        (status = 200, body = VectorStoreObject),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
) -> Result<Json<VectorStoreObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    let vs = app_state
        .vector_store_service
        .get_vector_store(vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_vector_store_object(&vs)))
}

#[utoipa::path(
    post,
    path = "/v1/vector_stores/{vector_store_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store to modify")
    ),
    request_body = ModifyVectorStoreRequest,
    responses(
        (status = 200, body = VectorStoreObject),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn modify_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<ModifyVectorStoreRequest>,
) -> Result<Json<VectorStoreObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    let metadata = body
        .metadata
        .as_ref()
        .map(|m| serde_json::to_value(m).unwrap_or_default());

    let expires_after_anchor = body.expires_after.as_ref().map(|ea| {
        serde_json::to_value(&ea.anchor)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "last_active_at".to_string())
    });
    let expires_after_days = body.expires_after.as_ref().map(|ea| ea.days);

    let vs = app_state
        .vector_store_service
        .update_vector_store(
            vs_uuid,
            api_key.workspace_id.0,
            &UpdateVectorStoreParams {
                name: body.name,
                expires_after_anchor,
                expires_after_days,
                metadata,
            },
        )
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_vector_store_object(&vs)))
}

#[utoipa::path(
    delete,
    path = "/v1/vector_stores/{vector_store_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store to delete")
    ),
    responses(
        (status = 200, body = VectorStoreDeleteResponse),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn delete_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
) -> Result<Json<VectorStoreDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    app_state
        .vector_store_service
        .delete_vector_store(vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(VectorStoreDeleteResponse {
        id: format!("{PREFIX_VS}{vs_uuid}"),
        object: "vector_store.deleted".to_string(),
        deleted: true,
    }))
}

// ---------------------------------------------------------------------------
// Vector Store Files
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/vector_stores/{vector_store_id}/files",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store")
    ),
    request_body = CreateVectorStoreFileRequest,
    responses(
        (status = 200, body = VectorStoreFileObject),
        (status = 404, body = ErrorResponse),
        (status = 409, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<CreateVectorStoreFileRequest>,
) -> Result<Json<VectorStoreFileObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&body.file_id, PREFIX_FILE)?;

    let chunking_strategy = body
        .chunking_strategy
        .as_ref()
        .and_then(|cs| serde_json::to_value(cs).ok());

    let attributes = body
        .attributes
        .as_ref()
        .map(|a| serde_json::to_value(a).unwrap_or_default());

    let vsf = app_state
        .vector_store_service
        .create_vector_store_file(CreateVectorStoreFileParams {
            vector_store_id: vs_uuid,
            file_id: file_uuid,
            workspace_id: api_key.workspace_id.0,
            batch_id: None,
            chunking_strategy,
            attributes,
        })
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_vector_store_file_object(&vsf)))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}/files",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ListQueryParams,
    ),
    responses(
        (status = 200, body = VectorStoreFileListResponse),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_store_files(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<ListQueryParams>,
) -> Result<Json<VectorStoreFileListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let workspace_id = api_key.workspace_id.0;
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let order = params.order.unwrap_or_else(|| "desc".to_string());

    let after = params
        .after
        .as_deref()
        .map(|s| parse_uuid_with_prefix(s, PREFIX_VSF))
        .transpose()?;

    let before = params
        .before
        .as_deref()
        .map(|s| parse_uuid_with_prefix(s, PREFIX_VSF))
        .transpose()?;

    let files = app_state
        .vector_store_service
        .list_vector_store_files(
            vs_uuid,
            workspace_id,
            &ListParams {
                workspace_id,
                limit: limit + 1,
                order,
                after,
                before,
                filter: None,
            },
        )
        .await
        .map_err(map_service_error)?;

    let has_more = files.len() > limit as usize;
    let files_to_return: Vec<_> = files.into_iter().take(limit as usize).collect();

    let data: Vec<VectorStoreFileObject> = files_to_return
        .iter()
        .map(to_vector_store_file_object)
        .collect();
    let first_id = data.first().map(|o| o.id.clone());
    let last_id = data.last().map(|o| o.id.clone());

    Ok(Json(VectorStoreFileListResponse {
        object: "list".to_string(),
        data,
        first_id,
        last_id,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}/files/{file_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("file_id" = String, Path, description = "The ID of the file")
    ),
    responses(
        (status = 200, body = VectorStoreFileObject),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, file_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&file_id, PREFIX_VSF)?;

    let vsf = app_state
        .vector_store_service
        .get_vector_store_file(file_uuid, vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_vector_store_file_object(&vsf)))
}

#[utoipa::path(
    post,
    path = "/v1/vector_stores/{vector_store_id}/files/{file_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("file_id" = String, Path, description = "The ID of the file to update")
    ),
    request_body = UpdateVectorStoreFileAttributesRequest,
    responses(
        (status = 200, body = VectorStoreFileObject),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn update_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, file_id)): Path<(String, String)>,
    Json(body): Json<UpdateVectorStoreFileAttributesRequest>,
) -> Result<Json<VectorStoreFileObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&file_id, PREFIX_VSF)?;

    let attributes_value = serde_json::to_value(&body.attributes).unwrap_or_default();

    let vsf = app_state
        .vector_store_service
        .update_vector_store_file_attributes(
            file_uuid,
            vs_uuid,
            api_key.workspace_id.0,
            attributes_value,
        )
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_vector_store_file_object(&vsf)))
}

#[utoipa::path(
    delete,
    path = "/v1/vector_stores/{vector_store_id}/files/{file_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("file_id" = String, Path, description = "The ID of the file to delete")
    ),
    responses(
        (status = 200, body = VectorStoreFileDeleteResponse),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn delete_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, file_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&file_id, PREFIX_VSF)?;

    app_state
        .vector_store_service
        .delete_vector_store_file(file_uuid, vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(VectorStoreFileDeleteResponse {
        id: format!("{PREFIX_VSF}{file_uuid}"),
        object: "vector_store.file.deleted".to_string(),
        deleted: true,
    }))
}

// ---------------------------------------------------------------------------
// Vector Store File Batches
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/vector_stores/{vector_store_id}/file_batches",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store")
    ),
    request_body = CreateVectorStoreFileBatchRequest,
    responses(
        (status = 200, body = VectorStoreFileBatchObject),
        (status = 400, body = ErrorResponse),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store_file_batch(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<CreateVectorStoreFileBatchRequest>,
) -> Result<Json<VectorStoreFileBatchObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let workspace_id = api_key.workspace_id.0;

    let file_ids = body.file_ids.unwrap_or_default();
    if file_ids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "file_ids must not be empty".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let parsed_file_ids: Vec<Uuid> = file_ids
        .iter()
        .map(|fid| parse_uuid_with_prefix(fid, PREFIX_FILE))
        .collect::<Result<Vec<_>, _>>()?;

    let chunking_strategy = body
        .chunking_strategy
        .as_ref()
        .and_then(|cs| serde_json::to_value(cs).ok());

    let attributes = body
        .attributes
        .as_ref()
        .map(|a| serde_json::to_value(a).unwrap_or_default());

    let batch = app_state
        .vector_store_service
        .create_file_batch(CreateVectorStoreFileBatchParams {
            vector_store_id: vs_uuid,
            workspace_id,
            file_ids: parsed_file_ids,
            chunking_strategy,
            attributes,
        })
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_file_batch_object(&batch)))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}/file_batches/{batch_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("batch_id" = String, Path, description = "The ID of the file batch")
    ),
    responses(
        (status = 200, body = VectorStoreFileBatchObject),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store_file_batch(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, batch_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileBatchObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let batch_uuid = parse_uuid_with_prefix(&batch_id, PREFIX_VSFB)?;

    let batch = app_state
        .vector_store_service
        .get_file_batch(batch_uuid, vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_file_batch_object(&batch)))
}

#[utoipa::path(
    post,
    path = "/v1/vector_stores/{vector_store_id}/file_batches/{batch_id}/cancel",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("batch_id" = String, Path, description = "The ID of the file batch to cancel")
    ),
    responses(
        (status = 200, body = VectorStoreFileBatchObject),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn cancel_vector_store_file_batch(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, batch_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileBatchObject>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let batch_uuid = parse_uuid_with_prefix(&batch_id, PREFIX_VSFB)?;

    let batch = app_state
        .vector_store_service
        .cancel_file_batch(batch_uuid, vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(to_file_batch_object(&batch)))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}/file_batches/{batch_id}/files",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("batch_id" = String, Path, description = "The ID of the file batch"),
        BatchFileListQueryParams,
    ),
    responses(
        (status = 200, body = VectorStoreFileListResponse),
        (status = 404, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_store_file_batch_files(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, batch_id)): Path<(String, String)>,
    axum::extract::Query(params): axum::extract::Query<BatchFileListQueryParams>,
) -> Result<Json<VectorStoreFileListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let batch_uuid = parse_uuid_with_prefix(&batch_id, PREFIX_VSFB)?;
    let workspace_id = api_key.workspace_id.0;
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let order = params.order.unwrap_or_else(|| "desc".to_string());

    let after = params
        .after
        .as_deref()
        .map(|s| parse_uuid_with_prefix(s, PREFIX_VSF))
        .transpose()?;

    let before = params
        .before
        .as_deref()
        .map(|s| parse_uuid_with_prefix(s, PREFIX_VSF))
        .transpose()?;

    let files = app_state
        .vector_store_service
        .list_file_batch_files(
            batch_uuid,
            vs_uuid,
            workspace_id,
            &ListParams {
                workspace_id,
                limit: limit + 1,
                order,
                after,
                before,
                filter: params.filter.and_then(|f| {
                    serde_json::to_value(&f)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                }),
            },
        )
        .await
        .map_err(map_service_error)?;

    let has_more = files.len() > limit as usize;
    let files_to_return: Vec<_> = files.into_iter().take(limit as usize).collect();

    let data: Vec<VectorStoreFileObject> = files_to_return
        .iter()
        .map(to_vector_store_file_object)
        .collect();
    let first_id = data.first().map(|o| o.id.clone());
    let last_id = data.last().map(|o| o.id.clone());

    Ok(Json(VectorStoreFileListResponse {
        object: "list".to_string(),
        data,
        first_id,
        last_id,
        has_more,
    }))
}
