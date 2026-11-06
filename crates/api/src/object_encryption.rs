use axum::{
    routing::{get, post},
    Router,
};
use database::DbPool;
use services::files::{encryption::ObjectKeyRing, storage::StorageTrait};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use uuid::Uuid;

mod endpoints;
mod job_error;

const OBJECT_JOB_KIND_FILTER: &str = "scope->>'kind'='objects'";
// Exposed for integration tests to exercise the real lease SQL with opaque jobs.
#[doc(hidden)]
pub mod lease;
mod object_processing;
mod progress;
mod types;
mod worker;
pub(crate) use endpoints::{
    __path_cancel_job, __path_create_job, __path_get_job, __path_get_keys, __path_scan,
    __path_verify,
};
pub use endpoints::{cancel_job, create_job, get_job, get_keys, scan, verify};
pub use types::{
    ObjectCreateJobRequest, ObjectJobResponse, ObjectScanRequest, ObjectScanResponse,
    ObjectVerifyRequest,
};

#[derive(Clone)]
pub struct ObjectEncryptionState {
    pool: DbPool,
    storage: Arc<dyn StorageTrait>,
    ring: Arc<ObjectKeyRing>,
    worker_permit: Arc<Semaphore>,
    recovery_started: Arc<AtomicBool>,
    active_jobs: Arc<Mutex<HashSet<Uuid>>>,
    recovery_interval: Duration,
    lease_retry_interval: Duration,
    storage_retry_interval: Duration,
    storage_timeout: Duration,
    job_scoped_leases: bool,
    max_bytes: u64,
    write_enabled: bool,
}

impl ObjectEncryptionState {
    pub fn new(pool: DbPool, storage: Arc<dyn StorageTrait>, ring: Arc<ObjectKeyRing>) -> Self {
        Self {
            pool,
            storage,
            ring,
            worker_permit: Arc::new(Semaphore::new(1)),
            recovery_started: Arc::new(AtomicBool::new(false)),
            active_jobs: Arc::new(Mutex::new(HashSet::new())),
            recovery_interval: Duration::from_secs(30),
            lease_retry_interval: Duration::from_millis(250),
            storage_retry_interval: Duration::from_millis(25),
            storage_timeout: Duration::from_secs(30),
            job_scoped_leases: false,
            max_bytes: 256 * 1024 * 1024,
            write_enabled: true,
        }
    }

    #[doc(hidden)]
    pub fn with_recovery_interval(mut self, interval: Duration) -> Self {
        self.recovery_interval = interval;
        self
    }

    pub fn with_job_options(mut self, max_bytes: u64, write_enabled: bool) -> Self {
        self.max_bytes = max_bytes;
        self.write_enabled = write_enabled;
        self
    }

    #[doc(hidden)]
    pub fn with_lease_retry_interval(mut self, interval: Duration) -> Self {
        self.lease_retry_interval = interval;
        self
    }

    #[doc(hidden)]
    pub fn with_storage_retry_interval(mut self, interval: Duration) -> Self {
        self.storage_retry_interval = interval;
        self
    }

    #[doc(hidden)]
    pub fn with_job_scoped_leases(mut self) -> Self {
        self.job_scoped_leases = true;
        self
    }

    #[doc(hidden)]
    pub fn recover_job(&self, id: Uuid) {
        spawn_job(self.clone().with_job_scoped_leases(), id);
    }

    pub fn recover_jobs(&self) {
        if self.recovery_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(state.recovery_interval);
            loop {
                interval.tick().await;
                let result = async {
                    let client = state.pool.get().await?;
                    let rows = client.query(
                        &format!("SELECT id FROM database_encryption_jobs WHERE {OBJECT_JOB_KIND_FILTER} AND status IN ('queued','running') ORDER BY created_at"), &[]
                    ).await?;
                    drop(client);
                    for row in rows {
                        spawn_job(state.clone(), row.get(0));
                    }
                    anyhow::Ok(())
                }.await;
                if let Err(error) = result {
                    tracing::error!(
                        error_class = "object_encryption_job_recovery_failed",
                        error = %error,
                        "Failed to recover object encryption jobs"
                    );
                }
            }
        });
    }
}

pub fn router() -> Router {
    Router::new()
        .route("/admin/object-encryption/keys", get(get_keys))
        .route("/admin/object-encryption/scan", post(scan))
        .route("/admin/object-encryption/jobs", post(create_job))
        .route("/admin/object-encryption/jobs/{id}", get(get_job))
        .route(
            "/admin/object-encryption/jobs/{id}/cancel",
            post(cancel_job),
        )
        .route("/admin/object-encryption/verify", post(verify))
}

fn spawn_job(state: ObjectEncryptionState, id: Uuid) {
    if !state
        .active_jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id)
    {
        return;
    }
    tokio::spawn(async move {
        let worker_state = state.clone();
        let result = tokio::spawn(async move { worker::run_job(&worker_state, id).await }).await;
        let error_class = match result {
            Ok(Ok(())) => None,
            Ok(Err(_)) => Some("object_worker_failed"),
            Err(_) => Some("object_worker_panicked"),
        };
        if let Some(error_class) = error_class {
            tracing::error!(job_id=%id, error_class, "Object encryption job failed");
        }
        state
            .active_jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
    });
}

#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod job_missing_tests;
#[cfg(test)]
mod missing_tests;

#[cfg(test)]
mod worker_tests;

#[cfg(test)]
mod worker_control_tests;
#[cfg(test)]
mod worker_test_storage;

#[cfg(test)]
mod lease_tests;

#[cfg(test)]
mod worker_warning_tests;
