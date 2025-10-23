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
