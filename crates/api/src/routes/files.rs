use crate::{
    middleware::auth::AuthenticatedApiKey,
    models::{ErrorResponse, FileListResponse, FileUploadResponse},
    routes::api::AppState,
};
use axum::{
    extract::{Multipart, State},
    http::StatusCode,
    response::Json,
    Extension,
};
use database::repositories::FileRepository;
use services::files::{
    calculate_expires_at, generate_storage_key, storage::S3Storage, validate_encoding,
    validate_mime_type, validate_purpose,
};
use tracing::{debug, error};

const MAX_FILE_SIZE: u64 = 512 * 1024 * 1024; // 512 MB

#[utoipa::path(
    post,
    path = "/v1/files",
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
    Extension(api_key): Extension<AuthenticatedApiKey>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FileUploadResponse>), (StatusCode, Json<ErrorResponse>)> {
    debug!("File upload request from workspace: {}", api_key.workspace.id.0);

    let mut file_data: Option<Vec<u8>> = None;
    let mut filename: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut purpose: Option<String> = None;
    let mut expires_after_anchor: Option<String> = None;
    let mut expires_after_seconds: Option<i64> = None;

    // Parse multipart form data
    while let Ok(Some(field)) = multipart.next_field().await {
        let field_name = field.name().unwrap_or("").to_string();

        match field_name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                content_type = field.content_type().map(|s| s.to_string());
                let data = field.bytes().await.map_err(|e| {
                    error!("Failed to read file data: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            format!("Failed to read file data: {}", e),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?;

                // Check file size
                if data.len() as u64 > MAX_FILE_SIZE {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        Json(ErrorResponse::new(
                            format!(
                                "File too large: {} bytes (max: {} bytes)",
                                data.len(),
                                MAX_FILE_SIZE
                            ),
                            "invalid_request_error".to_string(),
                        )),
                    ));
                }

                file_data = Some(data.to_vec());
            }
            "purpose" => {
                let text = field.text().await.map_err(|e| {
                    error!("Failed to read purpose: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            format!("Failed to read purpose: {}", e),
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
                            format!("Failed to read expires_after[anchor]: {}", e),
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
                            format!("Failed to read expires_after[seconds]: {}", e),
                            "invalid_request_error".to_string(),
                        )),
                    )
                })?;
                expires_after_seconds = Some(text.parse::<i64>().map_err(|e| {
                    error!("Failed to parse expires_after[seconds]: {}", e);
                    (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse::new(
                            format!("Invalid expires_after[seconds]: must be an integer"),
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

    // Validate purpose
    validate_purpose(&purpose).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(e.to_string(), "invalid_request_error".to_string())),
        )
    })?;

    // Validate MIME type
    validate_mime_type(&content_type).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(e.to_string(), "invalid_request_error".to_string())),
        )
    })?;

    // Validate encoding for text files
    validate_encoding(&content_type, &file_data).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(e.to_string(), "invalid_request_error".to_string())),
        )
    })?;

    // Calculate expires_at if expires_after is provided
    let created_at = chrono::Utc::now();
    let expires_at = if let (Some(anchor), Some(seconds)) =
        (expires_after_anchor, expires_after_seconds)
    {
        Some(calculate_expires_at(&anchor, seconds, created_at).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(e.to_string(), "invalid_request_error".to_string())),
            )
        })?)
    } else {
        None
    };

    // Generate file ID and storage key
    let file_id = uuid::Uuid::new_v4();
    let storage_key = generate_storage_key(api_key.workspace.id.0, file_id, &filename);

    // Initialize S3 storage
    let s3_config = aws_config::load_from_env().await;
    let s3_client = aws_sdk_s3::Client::new(&s3_config);
    let s3_bucket = app_state.config.s3.bucket.clone();
    let s3_encryption_key = app_state.config.s3.encryption_key.clone();

    let storage = S3Storage::new(s3_client, s3_bucket, s3_encryption_key);

    // Upload file to S3
    storage
        .upload(&storage_key, file_data.clone(), &content_type)
        .await
        .map_err(|e| {
            error!("Failed to upload file to S3: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    format!("Failed to upload file: {}", e),
                    "internal_error".to_string(),
                )),
            )
        })?;

    // Create file record in database
    let file_repo = FileRepository::new(app_state.db_pool.clone());
    let file = file_repo
        .create(
            filename.clone(),
            file_data.len() as i64,
            content_type,
            purpose.clone(),
            storage_key,
            api_key.workspace.id.0,
            Some(api_key.api_key.created_by_user_id.0),
            expires_at,
        )
        .await
        .map_err(|e| {
            error!("Failed to create file record: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    format!("Failed to save file metadata: {}", e),
                    "internal_error".to_string(),
                )),
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
    path = "/v1/files",
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
    Extension(api_key): Extension<AuthenticatedApiKey>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<crate::models::FileListResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("List files request from workspace: {}", api_key.workspace.id.0);

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

    let order = params
        .get("order")
        .map(|s| s.as_str())
        .unwrap_or("desc");

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
                Json(ErrorResponse::new(e.to_string(), "invalid_request_error".to_string())),
            )
        })?;
    }

    // Query files from database
    let file_repo = FileRepository::new(app_state.db_pool.clone());
    let files = file_repo
        .list_with_pagination(api_key.workspace.id.0, after, limit + 1, order, purpose)
        .await
        .map_err(|e| {
            error!("Failed to list files: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    format!("Failed to list files: {}", e),
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
    path = "/v1/files/{file_id}",
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
    Extension(api_key): Extension<AuthenticatedApiKey>,
    axum::extract::Path(file_id): axum::extract::Path<String>,
) -> Result<Json<FileUploadResponse>, (StatusCode, Json<ErrorResponse>)> {
    debug!("Get file request: {} from workspace: {}", file_id, api_key.workspace.id.0);

    // Parse file ID (remove "file-" prefix if present)
    let id_str = file_id.strip_prefix("file-").unwrap_or(&file_id);
    let file_uuid = uuid::Uuid::parse_str(id_str).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                format!("Invalid file ID format: {}", file_id),
                "invalid_request_error".to_string(),
            )),
        )
    })?;

    // Query file from database (with workspace authorization check)
    let file_repo = FileRepository::new(app_state.db_pool.clone());
    let file = file_repo
        .get_by_id_and_workspace(file_uuid, api_key.workspace.id.0)
        .await
        .map_err(|e| {
            error!("Failed to retrieve file: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    format!("Failed to retrieve file: {}", e),
                    "internal_error".to_string(),
                )),
            )
        })?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse::new(
                    format!("File not found: {}", file_id),
                    "not_found_error".to_string(),
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
