use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::Value;
use url::form_urlencoded;
use uuid::Uuid;

use crate::{
    models::{
        CreateVectorStoreFileBatchRequest, CreateVectorStoreFileRequest, CreateVectorStoreRequest,
        ErrorResponse, ModifyVectorStoreRequest, UpdateVectorStoreFileAttributesRequest,
        VectorStoreDeleteResponse, VectorStoreFileBatchObject, VectorStoreFileDeleteResponse,
        VectorStoreFileListResponse, VectorStoreFileObject, VectorStoreListResponse,
        VectorStoreObject, VectorStoreSearchRequest, VectorStoreSearchResponse,
    },
    routes::api::AppState,
};
use services::{
    id_prefixes::{PREFIX_FILE, PREFIX_VS, PREFIX_VSFB},
    rag::RagError,
    vector_stores::VectorStoreServiceError,
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
    let (status, error_type, message) = match &e {
        VectorStoreServiceError::NotFound => (StatusCode::NOT_FOUND, "not_found", e.to_string()),
        VectorStoreServiceError::FileNotFound => {
            (StatusCode::NOT_FOUND, "not_found", e.to_string())
        }
        VectorStoreServiceError::InvalidParams(_) => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            e.to_string(),
        ),
        VectorStoreServiceError::RagError(rag_err) => match rag_err {
            RagError::ApiError { status, body } => {
                let http_status = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                // Forward 4xx errors from RAG as-is, map 5xx to 502
                if http_status.is_client_error() {
                    (http_status, "invalid_request_error", body.clone())
                } else {
                    (
                        StatusCode::BAD_GATEWAY,
                        "server_error",
                        "RAG service error".to_string(),
                    )
                }
            }
            RagError::NotConfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "RAG service not configured".to_string(),
            ),
            _ => (
                StatusCode::BAD_GATEWAY,
                "server_error",
                "RAG service unavailable".to_string(),
            ),
        },
        VectorStoreServiceError::RepositoryError(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "An internal error occurred".to_string(),
        ),
    };

    if status.is_client_error() {
        tracing::warn!("Vector store operation failed: {}", e);
    } else {
        tracing::error!("Vector store operation failed: {}", e);
    }

    (
        status,
        Json(ErrorResponse::new(message, error_type.to_string())),
    )
}

// ---------------------------------------------------------------------------
// Query parameter structs
// ---------------------------------------------------------------------------

/// Query parameters for list endpoints
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct ListQueryParams {
    /// A limit on the number of objects to be returned.
    #[serde(default)]
    pub limit: Option<u32>,
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

/// Query parameters for listing files (supports filter)
#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct FileListQueryParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub before: Option<String>,
    /// Filter by file status.
    #[serde(default)]
    pub filter: Option<String>,
}

impl FileListQueryParams {
    /// Build a query string for proxying to the RAG service.
    /// Strips ID prefixes from cursor values.
    fn to_rag_query_string(&self, file_prefix: &str) -> String {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        if let Some(limit) = self.limit {
            serializer.append_pair("limit", &limit.clamp(1, 100).to_string());
        }
        if let Some(ref order) = self.order {
            serializer.append_pair("order", order);
        }
        if let Some(ref after) = self.after {
            let raw = after.strip_prefix(file_prefix).unwrap_or(after);
            serializer.append_pair("after", raw);
        }
        if let Some(ref before) = self.before {
            let raw = before.strip_prefix(file_prefix).unwrap_or(before);
            serializer.append_pair("before", raw);
        }
        if let Some(ref filter) = self.filter {
            serializer.append_pair("filter", filter);
        }
        serializer.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::FileListQueryParams;

    #[test]
    fn to_rag_query_string_encodes_and_strips_prefixes() {
        let params = FileListQueryParams {
            limit: Some(150),
            order: Some("asc&limit=1000".to_string()),
            after: Some("file-abc123".to_string()),
            before: Some("file-def456".to_string()),
            filter: Some("status=completed&x=y".to_string()),
        };

        let query = params.to_rag_query_string("file-");

        assert_eq!(
            query,
            "limit=100&order=asc%26limit%3D1000&after=abc123&before=def456&filter=status%3Dcompleted%26x%3Dy"
        );
    }
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
        (status = 200, description = "Vector store created successfully", body = VectorStoreObject),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let workspace_id = api_key.workspace_id.0;

    let response = app_state
        .vector_store_service
        .create_vector_store(workspace_id, body)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores",
    tag = "Vector Stores",
    params(ListQueryParams),
    responses(
        (status = 200, description = "List of vector stores", body = VectorStoreListResponse),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_stores(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    axum::extract::Query(params): axum::extract::Query<ListQueryParams>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
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

    let response = app_state
        .vector_store_service
        .list_vector_stores(
            workspace_id,
            &services::vector_stores::PaginationParams {
                limit,
                order,
                after,
                before,
            },
        )
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store to retrieve")
    ),
    responses(
        (status = 200, description = "Vector store retrieved successfully", body = VectorStoreObject),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    let response = app_state
        .vector_store_service
        .get_vector_store(vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "Vector store modified successfully", body = VectorStoreObject),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn modify_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    let response = app_state
        .vector_store_service
        .update_vector_store(vs_uuid, api_key.workspace_id.0, body)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}

#[utoipa::path(
    delete,
    path = "/v1/vector_stores/{vector_store_id}",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store to delete")
    ),
    responses(
        (status = 200, description = "Vector store deleted successfully", body = VectorStoreDeleteResponse),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn delete_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    let response = app_state
        .vector_store_service
        .delete_vector_store(vs_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Vector Store Search
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/vector_stores/{vector_store_id}/search",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store to search")
    ),
    request_body = VectorStoreSearchRequest,
    responses(
        (status = 200, description = "Search results", body = VectorStoreSearchResponse),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn search_vector_store(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    let response = app_state
        .vector_store_service
        .search_vector_store(vs_uuid, api_key.workspace_id.0, body)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "File attached to vector store", body = VectorStoreFileObject),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    // Extract and parse file_id from body
    let file_id_str = body
        .get("file_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "file_id is required".to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
        })?;
    let file_uuid = parse_uuid_with_prefix(file_id_str, PREFIX_FILE)?;

    let response = app_state
        .vector_store_service
        .attach_file(vs_uuid, file_uuid, api_key.workspace_id.0, body)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}/files",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        FileListQueryParams,
    ),
    responses(
        (status = 200, description = "List of vector store files", body = VectorStoreFileListResponse),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_store_files(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<FileListQueryParams>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let query_string = params.to_rag_query_string(PREFIX_FILE);

    let response = app_state
        .vector_store_service
        .list_vs_files(vs_uuid, api_key.workspace_id.0, &query_string)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "Vector store file retrieved successfully", body = VectorStoreFileObject),
        (status = 404, description = "File not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, file_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&file_id, PREFIX_FILE)?;

    let response = app_state
        .vector_store_service
        .get_vs_file(vs_uuid, file_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "File updated successfully", body = VectorStoreFileObject),
        (status = 404, description = "File not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn update_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, file_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&file_id, PREFIX_FILE)?;

    let response = app_state
        .vector_store_service
        .update_vs_file(vs_uuid, file_uuid, api_key.workspace_id.0, body)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "File deleted successfully", body = VectorStoreFileDeleteResponse),
        (status = 404, description = "File not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn delete_vector_store_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, file_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let file_uuid = parse_uuid_with_prefix(&file_id, PREFIX_FILE)?;

    let response = app_state
        .vector_store_service
        .detach_file(vs_uuid, file_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "File batch created successfully", body = VectorStoreFileBatchObject),
        (status = 400, description = "Invalid request parameters", body = ErrorResponse),
        (status = 404, description = "Vector store not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store_file_batch(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path(vector_store_id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;

    // Enforce mutual exclusivity: only one of file_ids or files is allowed
    let has_file_ids = body.get("file_ids").and_then(|v| v.as_array()).is_some();
    let has_files = body.get("files").and_then(|v| v.as_array()).is_some();

    if has_file_ids && has_files {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "file_ids and files are mutually exclusive; provide only one".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let file_id_strs: Vec<&Value> =
        if let Some(file_ids) = body.get("file_ids").and_then(|v| v.as_array()) {
            file_ids.iter().collect()
        } else if let Some(files) = body.get("files").and_then(|v| v.as_array()) {
            files.iter().collect()
        } else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "Either file_ids or files is required".to_string(),
                    "invalid_request_error".to_string(),
                )),
            ));
        };

    if file_id_strs.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "file_ids or files must not be empty".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let parsed_file_ids: Vec<Uuid> = file_id_strs
        .iter()
        .enumerate()
        .map(|(i, v)| {
            // If this is from file_ids, the value is a string ID
            // If this is from files, the value is an object with file_id field
            let id_str = if let Some(s) = v.as_str() {
                // Direct file_id from file_ids array
                s
            } else if let Some(file_spec) = v.as_object() {
                // Extract file_id from file spec object
                file_spec
                    .get("file_id")
                    .and_then(|fid| fid.as_str())
                    .ok_or_else(|| {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(ErrorResponse::new(
                                format!("files[{i}].file_id must be a string"),
                                "invalid_request_error".to_string(),
                            )),
                        )
                    })?
            } else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new(
                        "file_ids must contain strings or files must contain objects".to_string(),
                        "invalid_request_error".to_string(),
                    )),
                ));
            };
            parse_uuid_with_prefix(id_str, PREFIX_FILE)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let response = app_state
        .vector_store_service
        .create_file_batch(vs_uuid, &parsed_file_ids, api_key.workspace_id.0, body)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "File batch retrieved successfully", body = VectorStoreFileBatchObject),
        (status = 404, description = "File batch not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store_file_batch(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, batch_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let batch_uuid = parse_uuid_with_prefix(&batch_id, PREFIX_VSFB)?;

    let response = app_state
        .vector_store_service
        .get_file_batch(vs_uuid, batch_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
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
        (status = 200, description = "File batch cancelled successfully", body = VectorStoreFileBatchObject),
        (status = 404, description = "File batch not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn cancel_vector_store_file_batch(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, batch_id)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let batch_uuid = parse_uuid_with_prefix(&batch_id, PREFIX_VSFB)?;

    let response = app_state
        .vector_store_service
        .cancel_file_batch(vs_uuid, batch_uuid, api_key.workspace_id.0)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores/{vector_store_id}/file_batches/{batch_id}/files",
    tag = "Vector Stores",
    params(
        ("vector_store_id" = String, Path, description = "The ID of the vector store"),
        ("batch_id" = String, Path, description = "The ID of the file batch"),
        FileListQueryParams,
    ),
    responses(
        (status = 200, description = "List of files in the batch", body = VectorStoreFileListResponse),
        (status = 404, description = "File batch not found", body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_store_file_batch_files(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<ApiKey>,
    Path((vector_store_id, batch_id)): Path<(String, String)>,
    axum::extract::Query(params): axum::extract::Query<FileListQueryParams>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let vs_uuid = parse_uuid_with_prefix(&vector_store_id, PREFIX_VS)?;
    let batch_uuid = parse_uuid_with_prefix(&batch_id, PREFIX_VSFB)?;
    let query_string = params.to_rag_query_string(PREFIX_FILE);

    let response = app_state
        .vector_store_service
        .list_batch_files(vs_uuid, batch_uuid, api_key.workspace_id.0, &query_string)
        .await
        .map_err(map_service_error)?;

    Ok(Json(response))
}
