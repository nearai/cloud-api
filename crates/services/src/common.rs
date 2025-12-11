use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const API_KEY_PREFIX: &str = "sk-";
pub const API_KEY_LENGTH: usize = 35;

pub fn generate_api_key() -> String {
    format!(
        "{}{}",
        API_KEY_PREFIX,
        Uuid::new_v4().to_string().replace("-", "")
    )
}

pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn extract_api_key_prefix(key: &str) -> String {
    key[..10.min(key.len())].to_string()
}

pub fn is_valid_api_key_format(key: &str) -> bool {
    key.starts_with(API_KEY_PREFIX) && key.len() == API_KEY_LENGTH
}

/// Shared error types for repository operations across all domains.
/// These errors represent infrastructure concerns (database, connections, etc.)
/// rather than domain-specific business logic.
#[derive(thiserror::Error, Debug)]
pub enum RepositoryError {
    #[error("'{0}' does not exist")]
    NotFound(String),
    #[error("Cannot add this resource as it already exists")]
    AlreadyExists,
    #[error("Required field is missing: {0}")]
    RequiredFieldMissing(String),
    #[error("Referenced entity does not exist: {0}")]
    ForeignKeyViolation(String),
    #[error("Data validation failed: {0}")]
    ValidationFailed(String),
    #[error("Cannot delete due to existing dependencies: {0}")]
    DependencyExists(String),
    #[error("Transaction conflict, please retry")]
    TransactionConflict,
    #[error("Database connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Database authentication failed")]
    AuthenticationFailed,
    #[error("Database connection pool error: {0}")]
    PoolError(#[source] anyhow::Error),
    #[error("Database operation error: {0}")]
    DatabaseError(#[source] anyhow::Error),
    #[error("Data conversion error: {0}")]
    DataConversionError(#[source] anyhow::Error),
}

/// Convert a JSON schema value to inference provider ResponseFormat
///
/// Extracts name, description, schema, and strict fields from a JSON value
/// and constructs a ResponseFormat::JsonSchema.
///
/// This is shared by both /v1/responses (ResponseTextFormat) and /v1/chat/completions (ResponseFormatRequest)
pub fn convert_json_schema_to_response_format(
    json_schema: &serde_json::Value,
) -> inference_providers::models::ResponseFormat {
    let name = json_schema
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("response_schema")
        .to_string();

    let description = json_schema
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let schema = json_schema
        .get("schema")
        .cloned()
        .unwrap_or_else(|| json_schema.clone());

    let strict = json_schema.get("strict").and_then(|v| v.as_bool());

    inference_providers::models::ResponseFormat::JsonSchema {
        json_schema: inference_providers::models::JsonSchema {
            name,
            description,
            schema,
            strict,
        },
    }
}
