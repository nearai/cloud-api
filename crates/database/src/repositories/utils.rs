use services::common::RepositoryError;
use tokio_postgres::error::SqlState;

/// Convert tokio_postgres::Error to RepositoryError
pub fn map_db_error(err: tokio_postgres::Error) -> RepositoryError {
    // Handle database-level errors (connection, auth, etc.)
    if err.is_closed() {
        return RepositoryError::ConnectionFailed("Connection closed".to_string());
    }

    // Handle SQL state errors
    if let Some(db_err) = err.as_db_error() {
        let message = db_err.message();

        match db_err.code() {
            // Integrity constraint violations
            &SqlState::UNIQUE_VIOLATION => RepositoryError::AlreadyExists,
            &SqlState::FOREIGN_KEY_VIOLATION => {
                RepositoryError::ForeignKeyViolation(message.to_string())
            }
            &SqlState::NOT_NULL_VIOLATION => {
                RepositoryError::RequiredFieldMissing(message.to_string())
            }
            &SqlState::CHECK_VIOLATION => RepositoryError::ValidationFailed(message.to_string()),
            &SqlState::RESTRICT_VIOLATION => RepositoryError::DependencyExists(message.to_string()),

            // Transaction errors
            &SqlState::T_R_SERIALIZATION_FAILURE | &SqlState::T_R_DEADLOCK_DETECTED => {
                RepositoryError::TransactionConflict
            }

            // Connection/auth errors
            &SqlState::INVALID_PASSWORD | &SqlState::INVALID_AUTHORIZATION_SPECIFICATION => {
                RepositoryError::AuthenticationFailed
            }
            &SqlState::CONNECTION_EXCEPTION
            | &SqlState::CONNECTION_DOES_NOT_EXIST
            | &SqlState::CONNECTION_FAILURE => {
                RepositoryError::ConnectionFailed(message.to_string())
            }

            // Default case - wrap in generic database error
            _ => RepositoryError::DatabaseError(anyhow::anyhow!(
                "Database error ({}): {}",
                db_err.code().code(),
                message
            )),
        }
    } else {
        // Non-SQL errors (connection issues, etc.)
        RepositoryError::DatabaseError(err.into())
    }
}
