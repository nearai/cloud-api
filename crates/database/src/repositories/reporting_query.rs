use super::utils::map_db_error;
use services::common::RepositoryError;
use std::time::{Duration, Instant};
use tokio_postgres::Transaction;

pub const DEFAULT_REPORTING_STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);

pub fn reporting_deadline(
    maximum_duration: Duration,
    request_deadline: Option<Instant>,
) -> Result<Instant, RepositoryError> {
    let local_deadline = Instant::now()
        .checked_add(maximum_duration)
        .ok_or(RepositoryError::QueryTimeout)?;
    Ok(request_deadline
        .map(|deadline| deadline.min(local_deadline))
        .unwrap_or(local_deadline))
}

pub fn remaining_statement_timeout(deadline: Instant) -> Result<Duration, RepositoryError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(RepositoryError::QueryTimeout)?;
    let cancellation_headroom = (remaining / 10).min(Duration::from_millis(50));
    let statement_timeout = remaining.saturating_sub(cancellation_headroom);
    if statement_timeout.is_zero() {
        Err(RepositoryError::QueryTimeout)
    } else {
        Ok(statement_timeout)
    }
}

pub async fn configure_reporting_transaction(
    transaction: &Transaction<'_>,
    statement_timeout: Duration,
) -> Result<(), RepositoryError> {
    let timeout_millis = statement_timeout.as_millis().max(1);
    let timeout = format!("{timeout_millis}ms");
    transaction
        .query_one(
            "SELECT set_config('statement_timeout', $1, true), set_config('timezone', 'UTC', true)",
            &[&timeout],
        )
        .await
        .map_err(map_db_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_deadline_caps_repository_budget() {
        let request_deadline = Instant::now() + Duration::from_millis(200);
        let deadline = reporting_deadline(Duration::from_secs(10), Some(request_deadline)).unwrap();

        assert_eq!(deadline, request_deadline);
        let remaining = remaining_statement_timeout(deadline).unwrap();
        assert!(remaining < Duration::from_millis(200));
        assert!(remaining > Duration::from_millis(100));
    }

    #[test]
    fn expired_deadline_fails_before_query_execution() {
        let deadline = Instant::now() - Duration::from_millis(1);

        assert!(matches!(
            remaining_statement_timeout(deadline),
            Err(RepositoryError::QueryTimeout)
        ));
    }
}
