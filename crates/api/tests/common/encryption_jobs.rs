use database::DbPool;
use std::{cell::RefCell, time::Duration};
use uuid::Uuid;

/// Deletes only registered test jobs, including while assertions unwind.
/// Use with a multithread Tokio test so PostgreSQL I/O keeps running in Drop.
pub struct EncryptionJobsCleanup {
    pool: DbPool,
    ids: RefCell<Vec<Uuid>>,
}

impl EncryptionJobsCleanup {
    pub fn new(pool: DbPool) -> Self {
        Self {
            pool,
            ids: RefCell::new(Vec::new()),
        }
    }

    pub fn track(&self, id: Uuid) {
        self.ids.borrow_mut().push(id);
    }
}

impl Drop for EncryptionJobsCleanup {
    fn drop(&mut self) {
        let ids = self.ids.get_mut();
        if ids.is_empty() {
            return;
        }
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    self.pool
                        .get()
                        .await?
                        .execute(
                            "DELETE FROM database_encryption_jobs WHERE id=ANY($1::uuid[])",
                            &[&*ids],
                        )
                        .await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await
            })
        });
        if !matches!(result, Ok(Ok(()))) {
            if std::thread::panicking() {
                eprintln!("test encryption job cleanup failed: {result:?}");
            } else {
                panic!("test encryption job cleanup failed: {result:?}");
            }
        }
    }
}
