use std::sync::Arc;
use std::time::Duration;

use database::{
    admin_audit::{self, AdminRequest},
    DbPool,
};
use opentelemetry::{global, metrics::Counter};
use tokio::{sync::Semaphore, task::JoinHandle};

const MAX_IN_FLIGHT: usize = 4;
const MAX_PENDING: usize = 64;
const WRITE_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct AdminAuditStore {
    pool: DbPool,
    slots: Arc<Semaphore>,
    pending: Arc<Semaphore>,
    failures: Counter<u64>,
}

impl AdminAuditStore {
    pub(crate) fn from_env(pool: &DbPool) -> Option<Self> {
        let setting = std::env::var("ADMIN_REQUEST_AUDIT_ENABLED").ok()?;
        if setting != "1" && !setting.eq_ignore_ascii_case("true") {
            return None;
        }
        Some(Self::new(pool.clone()))
    }

    pub(super) fn new(pool: DbPool) -> Self {
        Self {
            pool,
            slots: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            pending: Arc::new(Semaphore::new(MAX_PENDING)),
            failures: global::meter("admin_audit")
                .u64_counter("cloud_api_admin_audit_store_failures")
                .with_description("Protected admin audit writes unconfirmed")
                .build(),
        }
    }

    pub(crate) fn submit(&self, event: AdminRequest) -> Option<JoinHandle<()>> {
        let Ok(pending) = self.pending.clone().try_acquire_owned() else {
            self.failures.add(1, &[]);
            return None;
        };
        let store = self.clone();
        Some(tokio::spawn(async move {
            let _pending = pending;
            let result = tokio::time::timeout(WRITE_DEADLINE, async {
                let Ok(_slot) = store.slots.acquire().await else {
                    return false;
                };
                admin_audit::insert(&store.pool, &event).await.is_ok()
            })
            .await;
            if !matches!(result, Ok(true)) {
                store.failures.add(1, &[]);
            }
        }))
    }
}
