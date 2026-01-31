use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const API_KEY_PREFIX: &str = "sk-";
pub const API_KEY_LENGTH: usize = 35;

/// Maximum serialized size for metadata blobs (e.g. conversation metadata, response metadata)
pub const MAX_METADATA_SIZE_BYTES: usize = 16 * 1024;

/// Validates that a JSON-serializable value doesn't exceed the maximum metadata size.
///
/// # Arguments
/// * `value` - The value to validate (must be serializable to JSON)
/// * `field_name` - Name of the field for error messages (e.g. "metadata", "message metadata")
///
/// # Returns
/// * `Ok(())` if the serialized size is within limits
/// * `Err(String)` with a descriptive error message if validation fails
pub fn validate_metadata_size<T: serde::Serialize>(
    value: &T,
    field_name: &str,
) -> Result<(), String> {
    let serialized = serde_json::to_string(value).map_err(|_| format!("Invalid {field_name}"))?;
    if serialized.len() > MAX_METADATA_SIZE_BYTES {
        return Err(format!(
            "{field_name} is too large (max {MAX_METADATA_SIZE_BYTES} bytes when serialized)"
        ));
    }
    Ok(())
}

/// Encryption header keys used in params.extra for passing encryption information
/// These keys are used to pass encryption headers from API routes to completion services.
/// Note: These use underscores (x_signing_algo) for params.extra HashMap keys,
/// while HTTP headers use hyphens (x-signing-algo).
pub mod encryption_headers {
    /// Key for signing algorithm in params.extra (corresponds to x-signing-algo HTTP header)
    pub const SIGNING_ALGO: &str = "x_signing_algo";
    /// Key for client public key in params.extra (corresponds to x-client-pub-key HTTP header)
    pub const CLIENT_PUB_KEY: &str = "x_client_pub_key";
    /// Key for model public key in params.extra (corresponds to x-model-pub-key HTTP header)
    pub const MODEL_PUB_KEY: &str = "x_model_pub_key";
}

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
