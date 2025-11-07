use crate::{
    models::{ErrorResponse, FileDeleteResponse, FileListResponse, FileUploadResponse},
    routes::api::AppState,
};
use axum::{
    body::Body,
    extract::{Multipart, State},
    http::{header, StatusCode},
    response::{Json, Response},
    Extension,
};
use services::files::calculate_expires_at;
use tracing::{debug, error};

const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024; // 512 MB

#[utoipa::path(
    post,
    path = "/files",
    tag = "Files",
    request_body(content_type = "multipart/form-data"),
    responses(
        (status = 201, description = "File uploaded successfully", body = FileUploadResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 413, description = "File too large", body = ErrorResponse)
    ),
    security(("api_key" = []))
)]
pub async fn upload_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FileUploadResponse>), (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "File upload request from workspace: {}",
        api_key.workspace_id.0
    );

    let mut file_data: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut purpose: Option<String> = None;
    let mut expires_after_anchor: Option<String> = None;
    let mut expires_after_seconds: Option<i64> = None;

    // Parse multipart form data
    while let Ok(Some(mut field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();

        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                content_type = field.content_type().map(|s| s.to_string());

                // Stream file and validate size incrementally to prevent DoS attacks
                let mut chunks = Vec::new();
                let mut total_size: u64 = 0;

                while let Ok(Some(chunk)) = field.chunk().await {
                    let chunk_size = chunk.len() as u64;
                    total_size += chunk_size;

                    // Check size limit BEFORE accumulating more data
                    if total_size > MAX_FILE_SIZE {
                        return Err((
                            StatusCode::PAYLOAD_TOO_LARGE,
                            Json(ErrorResponse::new(
                                format!("File too large: exceeds {} bytes limit", MAX_FILE_SIZE),
                                "invalid_request_error".to_string(),
                            )),
                        ));
                    }

                    chunks.push(chunk);
                }

                // Combine chunks into final vector only after validation
                let data: Vec<u8> = chunks.into_iter().flat_map(|c| c.to_vec()).collect();
                file_data = Some(data);
            }
            "purpose" => {
                let text = field.text().await.map_err(|e| {
                    error!("Failed to read purpose: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            format!("Failed to read purpose: {e}"),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?;
                purpose = Some(text);
            }
            "expires_after[anchor]" => {
                let text = field.text().await.map_err(|e| {
                    error!("Failed to read expires_after[anchor]: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            format!("Failed to read expires_after[anchor]: {e}"),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?;
                expires_after_anchor = Some(text);
            }
            "expires_after[seconds]" => {
                let text = field.text().await.map_err(|e| {
                    error!("Failed to read expires_after[seconds]: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            format!("Failed to read expires_after[seconds]: {e}"),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?;
                expires_after_seconds = Some(text.parse::<i64>().map_err(|e| {
                    error!("Failed to parse expires_after[seconds]: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            "Invalid expires_after[seconds]: must be an integer".to_string(),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?);
            }
            _ => {
                debug!("Ignoring unknown field: {}", field_name);
            }
        }
    }

    // Validate required fields
    let file_data = file_data.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Missing required field: file".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    let filename = filename.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "File must have a filename".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    let content_type = content_type.unwrap_or_else(|| "application/octet-stream".to_string());

    let purpose = purpose.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Missing required field: purpose".to_string(),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    // Calculate expires_at if expires_after is provided
    let created_at = chrono::Utc::now();
    let expires_at =
        if let (Some(anchor), Some(seconds)) = (expires_after_anchor, expires_after_seconds) {
            Some(
                calculate_expires_at(&anchor, seconds, created_at).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            e.to_string(),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?,
            )
        } else {
            None
        };

    // Use file service to handle upload (includes validation, storage, and DB operations)
    let file = app_state
        .files_service
        .upload_file(services::files::UploadFileParams {
            filename,
            file_data,
            content_type,
            purpose,
            workspace_id: api_key.workspace_id.0,
            uploaded_by_user_id: Some(api_key.created_by_user_id.0),
            expires_at,
        })
        .await
        .map_err(|e| {
            error!("Failed to upload file: {}", e);
            let (status, error_type) = match e {
                services::files::FileServiceError::FileTooLarge(_, _) => {
                    (StatusCode::PAYLOAD_TOO_LARGE, "invalid_request_error")
                }
                services::files::FileServiceError::InvalidFileType(_)
                | services::files::FileServiceError::InvalidPurpose(_)
                | services::files::FileServiceError::InvalidEncoding
                | services::files::FileServiceError::MissingField(_)
                | services::files::FileServiceError::InvalidExpiresAfter(_) => {
                    (StatusCode::BAD_REQUEST, "invalid_request_error")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            };
            (
                status,
                Json(ErrorResponse::new(e.to_string(), error_type.to_string())),
            )
        })?;

    debug!("File uploaded successfully: {}", file.id);

    // Build response with OpenAI-compatible format
    let response = FileUploadResponse {
        id: format!("file-{}", file.id),
        object: "file".to_string(),
        bytes: file.bytes,
        created_at: file.created_at.timestamp(),
        expires_at: file.expires_at.map(|dt| dt.timestamp()),
        filename: file.filename,
        purpose: file.purpose,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

#[utoipa::path(
    get,
    path = "/files",
    tag = "Files",
    params(
        ("after" = Option<String>, Query, description = "A cursor for pagination. Pass the file ID to fetch files after this one."),
        ("limit" = Option<i64>, Query, description = "Number of files to return (1-10000, default 10000)"),
        ("order" = Option<String>, Query, description = "Sort order by created_at timestamp: 'asc' or 'desc' (default 'desc')"),
        ("purpose" = Option<String>, Query, description = "Filter files by purpose")
    ),
    responses(
        (status = 200, description = "List of files retrieved successfully", body = FileListResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse)
    ),
    security(("api_key" = []))
)]
pub async fn list_files(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<crate::models::FileListResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "List files request from workspace: {}",
        api_key.workspace_id.0
    );

    // Parse query parameters
    let after = params.get("after").and_then(|s| {
        // Remove "file-" prefix if present
        let id_str = s.strip_prefix("file-").unwrap_or(s);
        uuid::Uuid::parse_str(id_str).ok()
    });

    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(10000)
        .clamp(1, 10000);

    let order = params.get("order").map(|s| s.as_str()).unwrap_or("desc");

    if order != "asc" && order != "desc" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "Invalid order parameter. Must be 'asc' or 'desc'".to_string(),
                "invalid_request_error".to_string(),
            )),
        ));
    }

    let purpose = params.get("purpose").map(|s| s.to_string());

    // Validate purpose if provided
    if let Some(ref p) = purpose {
        services::files::validate_purpose(p).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    e.to_string(),
                    "invalid_request_error".to_string(),
                )),
            )
        })?;
    }

    // Query files from database
    // Use file service to list files
    let files = app_state
        .files_service
        .list_files(api_key.workspace_id.0, after, limit + 1, order, purpose)
        .await
        .map_err(|e| {
            error!("Failed to list files: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    format!("Failed to list files: {e}"),
                    "internal_error".to_string(),
                )),
            )
        })?;

    // Determine if there are more results
    let has_more = files.len() > limit as usize;
    let files_to_return: Vec<_> = files.into_iter().take(limit as usize).collect();

    // Convert to response format
    let data: Vec<crate::models::FileUploadResponse> = files_to_return
        .iter()
        .map(|file| crate::models::FileUploadResponse {
            id: format!("file-{}", file.id),
            object: "file".to_string(),
            bytes: file.bytes,
            created_at: file.created_at.timestamp(),
            expires_at: file.expires_at.map(|dt| dt.timestamp()),
            filename: file.filename.clone(),
            purpose: file.purpose.clone(),
        })
        .collect();

    let first_id = data.first().map(|f| f.id.clone());
    let last_id = data.last().map(|f| f.id.clone());

    let response = crate::models::FileListResponse {
        object: "list".to_string(),
        data,
        first_id,
        last_id,
        has_more,
    };

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/files/{file_id}",
    tag = "Files",
    params(
        ("file_id" = String, Path, description = "The ID of the file to retrieve")
    ),
    responses(
        (status = 200, description = "File information retrieved successfully", body = FileUploadResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse)
    ),
    security(("api_key" = []))
)]
pub async fn get_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    axum::extract::Path(file_id): axum::extract::Path<String>,
) -> Result<Json<FileUploadResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Get file request: {} from workspace: {}",
        file_id, api_key.workspace_id.0
    );

    // Parse file ID (remove "file-" prefix if present)
    let id_str = file_id.strip_prefix("file-").unwrap_or(&file_id);
    let file_uuid = uuid::Uuid::parse_str(id_str).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                format!("Invalid file ID format: {file_id}"),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    // Use file service to get file (with workspace authorization check)
    let file = app_state
        .files_service
        .get_file(file_uuid, api_key.workspace_id.0)
        .await
        .map_err(|e| {
            error!("Failed to retrieve file: {}", e);
            let (status, error_type) = match e {
                services::files::FileServiceError::NotFound => {
                    (StatusCode::NOT_FOUND, "not_found_error")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            };
            (
                status,
                Json(ErrorResponse::new(
                    format!("Failed to retrieve file: {e}"),
                    error_type.to_string(),
                )),
            )
        })?;

    // Build response
    let response = FileUploadResponse {
        id: format!("file-{}", file.id),
        object: "file".to_string(),
        bytes: file.bytes,
        created_at: file.created_at.timestamp(),
        expires_at: file.expires_at.map(|dt| dt.timestamp()),
        filename: file.filename,
        purpose: file.purpose,
    };

    Ok(Json(response))
}

#[utoipa::path(
    delete,
    path = "/files/{file_id}",
    tag = "Files",
    params(
        ("file_id" = String, Path, description = "The ID of the file to delete")
    ),
    responses(
        (status = 200, description = "File deleted successfully", body = FileDeleteResponse),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse)
    ),
    security(("api_key" = []))
)]
pub async fn delete_file(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    axum::extract::Path(file_id): axum::extract::Path<String>,
) -> Result<Json<FileDeleteResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Delete file request: {} from workspace: {}",
        file_id, api_key.workspace_id.0
    );

    // Parse file ID (remove "file-" prefix if present)
    let id_str = file_id.strip_prefix("file-").unwrap_or(&file_id);
    let file_uuid = uuid::Uuid::parse_str(id_str).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                format!("Invalid file ID format: {file_id}"),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    // Use file service to delete file (includes workspace authorization check)
    let deleted = app_state
        .files_service
        .delete_file(file_uuid, api_key.workspace_id.0)
        .await
        .map_err(|e| {
            error!("Failed to delete file: {}", e);
            let (status, error_type) = match e {
                services::files::FileServiceError::NotFound => {
                    (StatusCode::NOT_FOUND, "not_found_error")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            };
            (
                status,
                Json(ErrorResponse::new(
                    format!("Failed to delete file: {e}"),
                    error_type.to_string(),
                )),
            )
        })?;

    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new(
                format!("File not found: {file_id}"),
                "not_found_error".to_string(),
            )),
        ));
    }

    debug!("File deleted successfully: {}", file_id);

    // Build response
    let response = FileDeleteResponse {
        id: format!("file-{file_uuid}"),
        object: "file".to_string(),
        deleted: true,
    };

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/files/{file_id}/content",
    tag = "Files",
    params(
        ("file_id" = String, Path, description = "The ID of the file to retrieve content from")
    ),
    responses(
        (status = 200, description = "File content retrieved successfully", content_type = "application/octet-stream"),
        (status = 400, description = "Bad request", body = ErrorResponse),
        (status = 401, description = "Unauthorized", body = ErrorResponse),
        (status = 404, description = "File not found", body = ErrorResponse)
    ),
    security(("api_key" = []))
)]
pub async fn get_file_content(
    State(app_state): State<AppState>,
    Extension(api_key): Extension<services::workspace::ApiKey>,
    axum::extract::Path(file_id): axum::extract::Path<String>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    debug!(
        "Get file content request: {} from workspace: {}",
        file_id, api_key.workspace_id.0
    );

    // Parse file ID (remove "file-" prefix if present)
    let id_str = file_id.strip_prefix("file-").unwrap_or(&file_id);
    let file_uuid = uuid::Uuid::parse_str(id_str).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                format!("Invalid file ID format: {file_id}"),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    // Use file service to get file metadata and content (with workspace authorization check)
    let (file, file_content) = app_state
        .files_service
        .get_file_content(file_uuid, api_key.workspace_id.0)
        .await
        .map_err(|e| {
            error!("Failed to retrieve file content: {}", e);
            let (status, error_type) = match e {
                services::files::FileServiceError::NotFound => {
                    (StatusCode::NOT_FOUND, "not_found_error")
                }
                _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            };
            (
                status,
                Json(ErrorResponse::new(
                    format!("Failed to retrieve file content: {e}"),
                    error_type.to_string(),
                )),
            )
        })?;

    debug!(
        "File content retrieved successfully: {} ({} bytes)",
        file_id,
        file_content.len()
    );

    // Build response with appropriate content-type
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, file.content_type)
        .header(header::CONTENT_LENGTH, file_content.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", file.filename),
        )
        .body(Body::from(file_content))
        .map_err(|e| {
            error!("Failed to build response: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "Failed to build response".to_string(),
                    "internal_error".to_string(),
                )),
            )
        })?;

    Ok(response)
}
