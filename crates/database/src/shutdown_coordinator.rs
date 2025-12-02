use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Shutdown coordinator for orchestrating graceful application shutdown
///
/// This coordinator manages the shutdown sequence with timeout protection,
/// ensuring all services are cleanly terminated in the correct order:
///
/// 1. **Stop Accepting Requests** (Handled by HTTP server)
///    - Server stops accepting new connections
///    - Existing requests drain (30s window)
///    - No new operations initiated
///
/// 2. **Cancel Background Tasks** (Managed by coordinator)
///    - Model discovery refresh task
///    - Database cluster monitoring task
///    - Other periodic background operations
///    - ~5-10 seconds allocated
///
/// 3. **Close Connections** (Managed by coordinator)
///    - Wait for active connections to return to pool
///    - Close all database connections
///    - Release provider pool resources
///    - ~10-15 seconds allocated
///
pub struct ShutdownCoordinator {
    /// Total shutdown timeout (e.g., 30 seconds)
    total_timeout: Duration,
    /// Start time of shutdown
    start_time: Option<Instant>,
}

/// Result of a shutdown stage
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownStageResult {
    /// Stage completed successfully
    Success,
    /// Stage completed but exceeded its recommended time
    SlowCompletion,
    /// Stage timed out and was forcefully terminated
    Timeout,
}

/// Shutdown stage configuration
pub struct ShutdownStage {
    /// Name of the stage
    pub name: &'static str,
    /// Recommended timeout for this stage
    pub timeout: Duration,
}

impl ShutdownCoordinator {
    /// Create a new shutdown coordinator with specified total timeout
    pub fn new(total_timeout: Duration) -> Self {
        Self {
            total_timeout,
            start_time: None,
        }
    }

    /// Start the shutdown sequence
    pub fn start(&mut self) {
        self.start_time = Some(Instant::now());
        info!(
            "Starting graceful shutdown sequence with timeout: {:?}",
            self.total_timeout
        );
    }

    /// Execute a shutdown stage with timeout protection
    ///
    /// Returns the result status of the stage and remaining time
    pub async fn execute_stage<F, Fut>(
        &self,
        stage: ShutdownStage,
        operation: F,
    ) -> (ShutdownStageResult, Duration)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let stage_start = Instant::now();
        let remaining = self.remaining_time();

        info!("Starting shutdown stage: {}", stage.name);
        debug!(
            "Stage: {}, timeout: {:?}, remaining: {:?}",
            stage.name, stage.timeout, remaining
        );

        if remaining.is_zero() {
            warn!("No time remaining for stage: {}", stage.name);
            return (ShutdownStageResult::Timeout, Duration::ZERO);
        }

        // Use the minimum of stage timeout and remaining global timeout
        let stage_timeout = stage.timeout.min(remaining);

        let result = tokio::time::timeout(stage_timeout, operation()).await;

        let elapsed = stage_start.elapsed();
        let result_status = match result {
            Ok(()) => {
                if elapsed > stage.timeout {
                    debug!(
                        "Stage '{}' completed but took longer than recommended: {:?} > {:?}",
                        stage.name, elapsed, stage.timeout
                    );
                    ShutdownStageResult::SlowCompletion
                } else {
                    ShutdownStageResult::Success
                }
            }
            Err(_) => {
                warn!(
                    "Stage '{}' exceeded timeout: {:?} (recommended: {:?})",
                    stage.name, elapsed, stage.timeout
                );
                ShutdownStageResult::Timeout
            }
        };

        let remaining_after = self.remaining_time();
        debug!(
            "Stage '{}' completed in {:?}. Status: {:?}. Remaining time: {:?}",
            stage.name, elapsed, result_status, remaining_after
        );

        (result_status, remaining_after)
    }

    /// Get remaining shutdown time
    pub fn remaining_time(&self) -> Duration {
        match self.start_time {
            Some(start) => {
                let elapsed = start.elapsed();
                if elapsed >= self.total_timeout {
                    Duration::ZERO
                } else {
                    self.total_timeout - elapsed
                }
            }
            None => self.total_timeout,
        }
    }

    /// Check if shutdown has exceeded total timeout
    pub fn has_exceeded_timeout(&self) -> bool {
        self.remaining_time().is_zero()
    }

    /// Get elapsed shutdown time
    pub fn elapsed_time(&self) -> Duration {
        self.start_time
            .map(|start| start.elapsed())
            .unwrap_or_default()
    }

    /// Get shutdown completion summary
    pub fn summary(&self) -> String {
        let elapsed = self.elapsed_time();
        let remaining = self.remaining_time();
        format!(
            "Shutdown progression - Elapsed: {:.2}s, Remaining: {:.2}s, Total: {:.2}s",
            elapsed.as_secs_f32(),
            remaining.as_secs_f32(),
            self.total_timeout.as_secs_f32()
        )
    }

    /// Finish shutdown and log completion summary
    pub fn finish(&self) {
        let elapsed = self.elapsed_time();
        let remaining = self.remaining_time();

        if remaining.is_zero() {
            warn!(
                "Shutdown completed with timeout exceeded. Total time: {:.2}s",
                elapsed.as_secs_f32()
            );
        } else {
            info!(
                "Graceful shutdown completed in {:.2}s. Remaining time: {:.2}s",
                elapsed.as_secs_f32(),
                remaining.as_secs_f32()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coordinator_creation() {
        let coordinator = ShutdownCoordinator::new(Duration::from_secs(30));
        assert_eq!(coordinator.remaining_time(), Duration::from_secs(30));
        assert!(!coordinator.has_exceeded_timeout());
    }

    #[test]
    fn test_remaining_time_before_start() {
        let coordinator = ShutdownCoordinator::new(Duration::from_secs(30));
        assert_eq!(coordinator.remaining_time(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn test_execute_stage_success() {
        let mut coordinator = ShutdownCoordinator::new(Duration::from_secs(30));
        coordinator.start();

        let stage = ShutdownStage {
            name: "test_stage",
            timeout: Duration::from_secs(5),
        };

        let (status, remaining) = coordinator
            .execute_stage(stage, || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
            })
            .await;

        assert_eq!(status, ShutdownStageResult::Success);
        assert!(remaining < Duration::from_secs(30));
        assert!(remaining > Duration::from_secs(29));
    }

    #[tokio::test]
    async fn test_execute_stage_timeout() {
        let mut coordinator = ShutdownCoordinator::new(Duration::from_secs(1));
        coordinator.start();

        let stage = ShutdownStage {
            name: "slow_stage",
            timeout: Duration::from_millis(100),
        };

        let (status, _remaining) = coordinator
            .execute_stage(stage, || async {
                tokio::time::sleep(Duration::from_secs(5)).await;
            })
            .await;

        assert_eq!(status, ShutdownStageResult::Timeout);
    }

    #[tokio::test]
    async fn test_multiple_stages() {
        let mut coordinator = ShutdownCoordinator::new(Duration::from_secs(10));
        coordinator.start();

        // Stage 1
        let stage1 = ShutdownStage {
            name: "stage_1",
            timeout: Duration::from_secs(2),
        };
        let (status1, remaining1) = coordinator
            .execute_stage(stage1, || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
            })
            .await;
        assert_eq!(status1, ShutdownStageResult::Success);

        // Stage 2
        let stage2 = ShutdownStage {
            name: "stage_2",
            timeout: Duration::from_secs(2),
        };
        let (status2, remaining2) = coordinator
            .execute_stage(stage2, || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
            })
            .await;
        assert_eq!(status2, ShutdownStageResult::Success);

        // Remaining time should decrease
        assert!(remaining2 < remaining1);
    }

    #[test]
    fn test_summary() {
        let coordinator = ShutdownCoordinator::new(Duration::from_secs(30));
        let summary = coordinator.summary();
        assert!(summary.contains("Elapsed"));
        assert!(summary.contains("Remaining"));
        assert!(summary.contains("Total"));
    }
}
