use axum::{extract::Path, http::StatusCode, Json};
use serde::Deserialize;

use crate::models::{
    CreateVectorStoreFileBatchRequest, CreateVectorStoreFileRequest, CreateVectorStoreRequest,
    ErrorResponse, ModifyVectorStoreRequest, UpdateVectorStoreFileAttributesRequest,
    VectorStoreDeleteResponse, VectorStoreFileBatchObject, VectorStoreFileDeleteResponse,
    VectorStoreFileListResponse, VectorStoreFileObject, VectorStoreFileStatus,
    VectorStoreListResponse, VectorStoreObject,
};

fn not_implemented() -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ErrorResponse::new(
            "Vector stores API is not yet implemented".to_string(),
            "not_implemented".to_string(),
        )),
    )
}

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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store(
    Json(_body): Json<CreateVectorStoreRequest>,
) -> Result<Json<VectorStoreObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
}

#[utoipa::path(
    get,
    path = "/v1/vector_stores",
    tag = "Vector Stores",
    params(ListQueryParams),
    responses(
        (status = 200, body = VectorStoreListResponse),
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_stores(
    axum::extract::Query(_params): axum::extract::Query<ListQueryParams>,
) -> Result<Json<VectorStoreListResponse>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store(
    Path(_vector_store_id): Path<String>,
) -> Result<Json<VectorStoreObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn modify_vector_store(
    Path(_vector_store_id): Path<String>,
    Json(_body): Json<ModifyVectorStoreRequest>,
) -> Result<Json<VectorStoreObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn delete_vector_store(
    Path(_vector_store_id): Path<String>,
) -> Result<Json<VectorStoreDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store_file(
    Path(_vector_store_id): Path<String>,
    Json(_body): Json<CreateVectorStoreFileRequest>,
) -> Result<Json<VectorStoreFileObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_store_files(
    Path(_vector_store_id): Path<String>,
    axum::extract::Query(_params): axum::extract::Query<ListQueryParams>,
) -> Result<Json<VectorStoreFileListResponse>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store_file(
    Path((_vector_store_id, _file_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn update_vector_store_file(
    Path((_vector_store_id, _file_id)): Path<(String, String)>,
    Json(_body): Json<UpdateVectorStoreFileAttributesRequest>,
) -> Result<Json<VectorStoreFileObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn delete_vector_store_file(
    Path((_vector_store_id, _file_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn create_vector_store_file_batch(
    Path(_vector_store_id): Path<String>,
    Json(_body): Json<CreateVectorStoreFileBatchRequest>,
) -> Result<Json<VectorStoreFileBatchObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn get_vector_store_file_batch(
    Path((_vector_store_id, _batch_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileBatchObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn cancel_vector_store_file_batch(
    Path((_vector_store_id, _batch_id)): Path<(String, String)>,
) -> Result<Json<VectorStoreFileBatchObject>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
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
        (status = 501, body = ErrorResponse),
    ),
    security(("api_key" = []))
)]
pub async fn list_vector_store_file_batch_files(
    Path((_vector_store_id, _batch_id)): Path<(String, String)>,
    axum::extract::Query(_params): axum::extract::Query<BatchFileListQueryParams>,
) -> Result<Json<VectorStoreFileListResponse>, (StatusCode, Json<ErrorResponse>)> {
    Err(not_implemented())
}
