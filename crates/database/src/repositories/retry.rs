use deadpool::managed::TimeoutType;
use services::common::RepositoryError;

pub fn should_retry(err: &RepositoryError) -> bool {
    match err {
        RepositoryError::TransactionConflict | RepositoryError::ConnectionFailed(_) => true,
        RepositoryError::PoolError(source) => !source.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<deadpool_postgres::PoolError>(),
                Some(deadpool_postgres::PoolError::Timeout(TimeoutType::Wait))
            )
        }),
        RepositoryError::NotFound(_)
        | RepositoryError::AlreadyExists
        | RepositoryError::RequiredFieldMissing(_)
        | RepositoryError::ForeignKeyViolation(_)
        | RepositoryError::ValidationFailed(_)
        | RepositoryError::DependencyExists(_)
        | RepositoryError::AuthenticationFailed
        | RepositoryError::QueryTimeout
        | RepositoryError::DatabaseError(_)
        | RepositoryError::DataConversionError(_) => false,
    }
}

/// Retry a database operation with exponential backoff
#[macro_export]
macro_rules! retry_db {
    ($operation:expr, $block:block) => {{
        use std::time::{Duration, Instant};

        const MAX_ATTEMPTS: u32 = 3;
        const INITIAL_BACKOFF_MS: u64 = 100;
        const BACKOFF_MULTIPLIER: f64 = 2.0;

        let mut attempt = 0u32;
        let mut backoff_ms = INITIAL_BACKOFF_MS;
        let start = Instant::now();

        loop {
            tracing::debug!(operation = $operation, "Starting database operation");

            attempt += 1;

            let result: Result<_, RepositoryError> = async $block.await;

            match result {
                Ok(value) => {
                    if attempt > 1 {
                        tracing::info!(
                            operation = $operation,
                            attempt = attempt,
                            duration_ms = start.elapsed().as_millis() as u64,
                            "Database operation succeeded after retry"
                        );
                    }
                    break Ok(value);
                }
                Err(err) if $crate::repositories::retry::should_retry(&err) && attempt < MAX_ATTEMPTS => {
                    tracing::warn!(
                        operation = $operation,
                        attempt = attempt,
                        max_attempts = MAX_ATTEMPTS,
                        error = %err,
                        backoff_ms = backoff_ms,
                        "Database operation failed, retrying"
                    );

                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms as f64 * BACKOFF_MULTIPLIER) as u64;
                }
                Err(err) => {
                    tracing::error!(
                        operation = $operation,
                        attempt = attempt,
                        duration_ms = start.elapsed().as_millis() as u64,
                        error = %err,
                        "Database operation failed permanently"
                    );
                    break Err(err);
                }
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::should_retry;
    use anyhow::Context;
    use deadpool::managed::TimeoutType;
    use services::common::RepositoryError;

    fn pool_timeout(kind: TimeoutType) -> RepositoryError {
        let error: Result<(), deadpool_postgres::PoolError> =
            Err(deadpool_postgres::PoolError::Timeout(kind));
        RepositoryError::PoolError(
            error
                .context("Failed to get database connection")
                .unwrap_err(),
        )
    }

    #[test]
    fn pool_wait_timeout_is_not_retried() {
        assert!(!should_retry(&pool_timeout(TimeoutType::Wait)));
    }

    #[test]
    fn other_pool_timeouts_and_connection_errors_are_retried() {
        assert!(should_retry(&pool_timeout(TimeoutType::Create)));
        assert!(should_retry(&pool_timeout(TimeoutType::Recycle)));
        assert!(should_retry(&RepositoryError::PoolError(
            deadpool_postgres::PoolError::Closed.into()
        )));
        assert!(should_retry(&RepositoryError::ConnectionFailed(
            "connection closed".into()
        )));
        assert!(should_retry(&RepositoryError::TransactionConflict));
    }
}
