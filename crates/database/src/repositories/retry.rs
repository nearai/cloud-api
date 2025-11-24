/// Retry a database operation with exponential backoff
#[macro_export]
macro_rules! retry_db {
    ($operation:expr, $block:block) => {{
        use std::time::{Duration, Instant};

        const MAX_ATTEMPTS: u32 = 3;
        const INITIAL_BACKOFF_MS: u64 = 100;
        const BACKOFF_MULTIPLIER: f64 = 2.0;

        let should_retry = |err: &RepositoryError| matches!(
            err,
            RepositoryError::TransactionConflict
                | RepositoryError::ConnectionFailed(_)
                | RepositoryError::PoolError(_)
        );

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
                Err(err) if should_retry(&err) && attempt < MAX_ATTEMPTS => {
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
