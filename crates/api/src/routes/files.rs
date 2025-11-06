use crate::{
    middleware::auth::AuthenticatedApiKey,
    models::{ErrorResponse, FileUploadResponse},
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
