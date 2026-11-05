use super::DATABASE_JOB_KIND_FILTER;
use std::{future::Future, time::Duration};

pub(super) async fn wait_between_batches<F, Fut>(
    delay: Duration,
    mut cancel: F,
) -> anyhow::Result<bool>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<bool>>,
{
    let deadline = tokio::time::Instant::now() + delay;
    loop {
        if cancel().await? {
            return Ok(true);
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(500))).await;
    }
}

/// Mark cancellation with a short-lived pooled connection; no connection or
/// transaction survives into a delay slice.
pub(super) async fn cancel_if_requested(
    pool: &database::DbPool,
    id: uuid::Uuid,
) -> anyhow::Result<bool> {
    let client = pool.get().await?;
    Ok(client.execute(
        &format!("UPDATE database_encryption_jobs SET status='cancelled',completed_at=NOW() WHERE {DATABASE_JOB_KIND_FILTER} AND id=$1 AND cancel_requested_at IS NOT NULL AND status IN ('queued','running')"),
        &[&id],
    ).await? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, time::Duration};
    use tokio::time::Instant;

    #[tokio::test]
    async fn cancellation_interrupts_a_large_batch_delay() {
        let started = Instant::now();
        let checks = Cell::new(0);
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            wait_between_batches(Duration::from_secs(10), || {
                checks.set(checks.get() + 1);
                std::future::ready(Ok(started.elapsed() >= Duration::from_millis(100)))
            }),
        )
        .await;
        assert!(result.expect("cancellation within two seconds").unwrap());
        assert!(checks.get() >= 2, "checked before and after sleeping");
    }

    #[tokio::test]
    async fn no_cancellation_preserves_the_full_delay() {
        let started = Instant::now();
        let cancelled =
            wait_between_batches(Duration::from_millis(20), || std::future::ready(Ok(false)))
                .await
                .unwrap();
        assert!(!cancelled);
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn cancellation_is_checked_before_sleep_even_with_zero_delay() {
        for delay in [Duration::ZERO, Duration::from_secs(10)] {
            let cancelled = tokio::time::timeout(
                Duration::from_millis(100),
                wait_between_batches(delay, || std::future::ready(Ok(true))),
            )
            .await
            .expect("pre-existing cancellation must not sleep")
            .unwrap();
            assert!(cancelled);
        }
    }
}
