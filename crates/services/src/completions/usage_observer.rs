//! Generic observer of a completion's final usage.
//!
//! A request-scoped component (for example a metering or settlement layer in
//! the API) hands an [`UsageObserver`] to the completion service on the
//! request. The service calls it exactly once when the completion finishes,
//! with the final token counts. Nothing here knows who the observer is.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use uuid::Uuid;

/// Final usage of one completion request. Contains counts and ids only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageReport {
    /// The request correlation id carried by the completion request.
    pub request_id: Uuid,
    /// `None` distinguishes an unknown count from a provider-reported zero.
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    /// Completion counts include a lower bound from content chunks without usage.
    pub partial: bool,
    /// `true` when the completion ended with usage; `false` when it ended
    /// without final usage (dropped stream, provider error, early failure).
    /// Incomplete reports may contain partial counts.
    pub completed: bool,
}

/// Receives the [`UsageReport`] of a finished completion.
///
/// `on_finished` is called at most once per request, possibly from a `Drop`
/// impl or a runtime worker thread. Implementors must keep it cheap and
/// non-blocking: do not `await`, block, or perform I/O inline; hand the
/// report to a channel or `tokio::spawn` if work is needed. The `Debug`
/// bound lets the request stay `Debug`; implementations must not print
/// customer content.
pub trait UsageObserver: Send + Sync + std::fmt::Debug {
    fn on_finished(&self, report: UsageReport);
}

/// Request-extension newtype the API layer carries so a middleware can pass
/// an observer to the route handler without services depending on axum.
#[derive(Clone, Debug)]
pub struct UsageObserverExt(pub Arc<dyn UsageObserver>);

/// Holds an optional observer and fires it at most once.
#[derive(Debug)]
pub(crate) struct UsageFireOnce {
    observer: Option<Arc<dyn UsageObserver>>,
    fired: AtomicBool,
    final_usage: AtomicBool,
}

impl UsageFireOnce {
    pub(crate) fn new(observer: Option<Arc<dyn UsageObserver>>) -> Self {
        Self {
            observer,
            fired: AtomicBool::new(false),
            final_usage: AtomicBool::new(false),
        }
    }

    /// Fire the observer unless it already fired. Returns whether this call fired.
    /// A panicking observer is contained so it cannot unwind through `Drop`.
    pub(crate) fn fire(&self, report: UsageReport) -> bool {
        let Some(observer) = &self.observer else {
            return false;
        };
        if self.fired.swap(true, Ordering::AcqRel) {
            return false;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.on_finished(report);
        }));
        if result.is_err() {
            tracing::error!(request_id = %report.request_id, "Usage observer panicked");
        }
        true
    }

    pub(crate) fn mark_final_usage(&self) {
        self.final_usage.store(true, Ordering::Release);
    }

    pub(crate) fn has_final_usage(&self) -> bool {
        self.final_usage.load(Ordering::Acquire)
    }
}

/// Fires `completed: false` on drop unless the report was sent or the
/// observer was handed to a longer-lived owner. Covers every early return.
#[derive(Debug)]
pub(crate) struct UsageReportGuard {
    once: UsageFireOnce,
    request_id: Uuid,
}

impl UsageReportGuard {
    pub(crate) fn new(observer: Option<Arc<dyn UsageObserver>>, request_id: Uuid) -> Self {
        Self {
            once: UsageFireOnce::new(observer),
            request_id,
        }
    }

    /// Report the final usage now; the drop fallback then does nothing.
    pub(crate) fn finish(&self, prompt_tokens: i32, completion_tokens: i32) {
        self.once.fire(UsageReport {
            request_id: self.request_id,
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(completion_tokens),
            completed: true,
            partial: false,
        });
    }

    /// Hand the observer to a new owner that will report; the guard stays silent.
    pub(crate) fn disarm(self) -> Option<Arc<dyn UsageObserver>> {
        let observer = self.once.observer.clone();
        self.once.fired.store(true, Ordering::Release);
        observer
    }
}

impl Drop for UsageReportGuard {
    fn drop(&mut self) {
        self.once.fire(UsageReport {
            request_id: self.request_id,
            prompt_tokens: None,
            completion_tokens: None,
            completed: false,
            partial: false,
        });
    }
}
