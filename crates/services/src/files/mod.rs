pub mod encryption;
pub mod storage;

use crate::common::RepositoryError;
use chrono::{DateTime, Utc};
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
    ("text/x-c", true),
    ("text/x-c++", true),
    ("text/x-csharp", true),
    ("text/css", true),
    ("application/msword", false),
    ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", false),
    ("text/x-golang", true),
    ("text/html", true),
    ("text/x-java", true),
    ("text/javascript", true),
    ("application/json", true),
    ("text/markdown", true),
    ("application/pdf", false),
    ("text/x-php", true),
    ("application/vnd.openxmlformats-officedocument.presentationml.presentation", false),
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
    let mime_type = content_type.split(';').next().unwrap_or(content_type).trim();

    if ALLOWED_MIME_TYPES.iter().any(|(allowed, _)| *allowed == mime_type) {
        Ok(())
    } else {
        Err(FileServiceError::InvalidFileType(content_type.to_string()))
    }
}

pub fn validate_encoding(content_type: &str, data: &[u8]) -> Result<(), FileServiceError> {
    // Extract just the MIME type
    let mime_type = content_type.split(';').next().unwrap_or(content_type).trim();

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
        "created_at" => {
            Ok(created_at + chrono::Duration::seconds(seconds))
        }
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

pub fn generate_storage_key(workspace_id: Uuid, file_id: Uuid, filename: &str) -> String {
    format!("{}/{}/{}", workspace_id, file_id, filename)
}
