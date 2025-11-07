pub mod encryption;
pub mod ports;
pub mod storage;

pub use ports::{File, FileRepositoryTrait};

use crate::common::RepositoryError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::sync::Arc;
use storage::StorageTrait;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum FileServiceError {
    #[error("File not found")]
    NotFound,
    #[error("File too large: {0} bytes (max: {1} bytes)")]
    FileTooLarge(u64, u64),
    #[error("Invalid file type: {0}")]
    InvalidFileType(String),
    #[error("Invalid file purpose: {0}")]
    InvalidPurpose(String),
    #[error("Storage error: {0}")]
    StorageError(String),
    #[error("Repository error: {0}")]
    RepositoryError(#[from] RepositoryError),
    #[error("Invalid encoding for text file. Expected UTF-8, UTF-16, or ASCII")]
    InvalidEncoding,
    #[error("Missing required field: {0}")]
    MissingField(String),
    #[error("Invalid expires_after parameter: {0}")]
    InvalidExpiresAfter(String),
    #[error("Encryption error: {0}")]
    EncryptionError(#[from] encryption::EncryptionError),
}

/// Allowed MIME types for file uploads with their encoding requirements
pub const ALLOWED_MIME_TYPES: &[(&str, bool)] = &[
    // (MIME type, requires_utf_encoding)
    ("application/octet-stream", false),
    ("text/x-c", true),
    ("text/x-c++", true),
    ("text/x-csharp", true),
    ("text/css", true),
    ("application/msword", false),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        false,
    ),
    ("text/x-golang", true),
    ("text/html", true),
    ("text/x-java", true),
    ("text/javascript", true),
    ("application/json", true),
    ("text/markdown", true),
    ("application/pdf", false),
    ("text/x-php", true),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        false,
    ),
    ("text/x-python", true),
    ("text/x-script.python", true),
    ("text/x-ruby", true),
    ("application/x-sh", true),
    ("text/x-tex", true),
    ("application/typescript", true),
    ("text/plain", true),
];

pub fn validate_mime_type(content_type: &str) -> Result<(), FileServiceError> {
    // Extract just the MIME type (remove charset if present)
    let mime_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    if ALLOWED_MIME_TYPES
        .iter()
        .any(|(allowed, _)| *allowed == mime_type)
    {
        Ok(())
    } else {
        Err(FileServiceError::InvalidFileType(content_type.to_string()))
    }
}

pub fn validate_encoding(content_type: &str, data: &[u8]) -> Result<(), FileServiceError> {
    // Extract just the MIME type
    let mime_type = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();

    // Check if this MIME type requires UTF encoding
    let requires_utf = ALLOWED_MIME_TYPES
        .iter()
        .find(|(allowed, _)| *allowed == mime_type)
        .map(|(_, req)| *req)
        .unwrap_or(false);

    if requires_utf {
        // Check for UTF-8
        if std::str::from_utf8(data).is_ok() {
            return Ok(());
        }

        // Check for UTF-16 LE BOM
        if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
            return Ok(());
        }

        // Check for UTF-16 BE BOM
        if data.len() >= 2 && data[0] == 0xFE && data[1] == 0xFF {
            return Ok(());
        }

        // Check if it's valid ASCII (subset of UTF-8)
        if data.iter().all(|&b| b < 128) {
            return Ok(());
        }

        return Err(FileServiceError::InvalidEncoding);
    }

    Ok(())
}

pub fn calculate_expires_at(
    anchor: &str,
    seconds: i64,
    created_at: DateTime<Utc>,
) -> Result<DateTime<Utc>, FileServiceError> {
    // Maximum expiration is 1 year (31536000 seconds)
    const MAX_EXPIRATION_SECONDS: i64 = 31536000;

    if seconds > MAX_EXPIRATION_SECONDS {
        return Err(FileServiceError::InvalidExpiresAfter(format!(
            "seconds cannot exceed {} (1 year)",
            MAX_EXPIRATION_SECONDS
        )));
    }

    if seconds <= 0 {
        return Err(FileServiceError::InvalidExpiresAfter(
            "seconds must be positive".to_string(),
        ));
    }

    match anchor {
        "created_at" => Ok(created_at + chrono::Duration::seconds(seconds)),
        _ => Err(FileServiceError::InvalidExpiresAfter(format!(
            "Invalid anchor: {}. Must be 'created_at'",
            anchor
        ))),
    }
}

pub fn validate_purpose(purpose: &str) -> Result<(), FileServiceError> {
    match purpose {
        "assistants" | "batch" | "fine-tune" | "vision" | "user_data" | "evals" => Ok(()),
        _ => Err(FileServiceError::InvalidPurpose(purpose.to_string())),
    }
}

pub fn generate_storage_key(workspace_id: Uuid, file_id: Uuid) -> String {
    format!("{}/{}", workspace_id, file_id)
}

/// File service trait for managing file uploads, downloads, and metadata
#[async_trait]
pub trait FileServiceTrait: Send + Sync {
    /// Upload a file to storage and create a database record
    async fn upload_file(
        &self,
        filename: String,
        file_data: Vec<u8>,
        content_type: String,
        purpose: String,
        workspace_id: Uuid,
        uploaded_by_user_id: Option<Uuid>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<File, FileServiceError>;

    /// Get file metadata by ID with workspace authorization
    async fn get_file(&self, file_id: Uuid, workspace_id: Uuid) -> Result<File, FileServiceError>;

    /// Get file content (metadata and raw bytes) with workspace authorization
    async fn get_file_content(
        &self,
        file_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<(File, Vec<u8>), FileServiceError>;

    /// List files with pagination
    async fn list_files(
        &self,
        workspace_id: Uuid,
        after: Option<Uuid>,
        limit: i64,
        order: &str,
        purpose: Option<String>,
    ) -> Result<Vec<File>, FileServiceError>;

    /// Delete a file from storage and database
    async fn delete_file(
        &self,
        file_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, FileServiceError>;
}

/// Implementation of the file service
pub struct FileServiceImpl {
    file_repository: Arc<dyn FileRepositoryTrait>,
    storage: Arc<dyn StorageTrait>,
}

impl FileServiceImpl {
    pub fn new(file_repository: Arc<dyn FileRepositoryTrait>, storage: Arc<dyn StorageTrait>) -> Self {
        Self {
            file_repository,
            storage,
        }
    }
}

#[async_trait]
impl FileServiceTrait for FileServiceImpl {
    async fn upload_file(
        &self,
        filename: String,
        file_data: Vec<u8>,
        content_type: String,
        purpose: String,
        workspace_id: Uuid,
        uploaded_by_user_id: Option<Uuid>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<File, FileServiceError> {
        // Validate MIME type
        validate_mime_type(&content_type)?;

        // Validate encoding for text files
        validate_encoding(&content_type, &file_data)?;

        // Validate purpose
        validate_purpose(&purpose)?;

        // Generate file ID and storage key
        let file_id = Uuid::new_v4();
        let storage_key = generate_storage_key(workspace_id, file_id);

        // Upload to storage (automatically encrypted)
        self.storage
            .upload(&storage_key, file_data.clone(), &content_type)
            .await
            .map_err(|e| FileServiceError::StorageError(e.to_string()))?;

        // Create database record
        let file = self
            .file_repository
            .create(
                filename,
                file_data.len() as i64,
                content_type,
                purpose,
                storage_key,
                workspace_id,
                uploaded_by_user_id,
                expires_at,
            )
            .await?;

        Ok(file)
    }

    async fn get_file(&self, file_id: Uuid, workspace_id: Uuid) -> Result<File, FileServiceError> {
        // Get file with workspace authorization
        let file = self
            .file_repository
            .get_by_id_and_workspace(file_id, workspace_id)
            .await?
            .ok_or(FileServiceError::NotFound)?;

        Ok(file)
    }

    async fn get_file_content(
        &self,
        file_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<(File, Vec<u8>), FileServiceError> {
        // Get file metadata with workspace authorization
        let file = self.get_file(file_id, workspace_id).await?;

        // Download from storage (automatically decrypted)
        let file_content = self
            .storage
            .download(&file.storage_key)
            .await
            .map_err(|e| FileServiceError::StorageError(e.to_string()))?;

        Ok((file, file_content))
    }

    async fn list_files(
        &self,
        workspace_id: Uuid,
        after: Option<Uuid>,
        limit: i64,
        order: &str,
        purpose: Option<String>,
    ) -> Result<Vec<File>, FileServiceError> {
        let files = self
            .file_repository
            .list_with_pagination(workspace_id, after, limit, order, purpose)
            .await?;

        Ok(files)
    }

    async fn delete_file(
        &self,
        file_id: Uuid,
        workspace_id: Uuid,
    ) -> Result<bool, FileServiceError> {
        // Get file with workspace authorization
        let file = self.get_file(file_id, workspace_id).await?;

        // Delete from storage
        self.storage
            .delete(&file.storage_key)
            .await
            .map_err(|e| FileServiceError::StorageError(e.to_string()))?;

        // Delete from database
        let deleted = self.file_repository.delete(file_id).await?;

        Ok(deleted)
    }
}
