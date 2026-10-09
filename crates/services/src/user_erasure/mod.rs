//! Admin-driven GDPR erasure of a user. See the user erasure design spec.

pub mod ports;

use std::sync::Arc;

use uuid::Uuid;

pub use ports::*;

#[derive(Debug, thiserror::Error)]
pub enum UserErasureError {
    #[error("user not found")]
    NotFound,
    #[error("erasure blocked")]
    Blocked(Vec<ErasureBlocker>),
    #[error("confirm_email does not match")]
    ConfirmEmailMismatch,
    #[error("erasure failed")]
    Internal(#[source] anyhow::Error),
}

#[derive(Debug, Clone)]
pub struct ErasureResult {
    pub user_id: Uuid,
    pub already_erased: bool,
    pub erased_organization_ids: Vec<Uuid>,
}

pub struct UserErasureService {
    repository: Arc<dyn UserErasureRepository>,
}

impl UserErasureService {
    pub fn new(repository: Arc<dyn UserErasureRepository>) -> Self {
        Self { repository }
    }

    pub async fn preview(&self, user_id: Uuid) -> Result<ErasurePlan, UserErasureError> {
        self.repository
            .plan(user_id)
            .await
            .map_err(UserErasureError::Internal)?
            .ok_or(UserErasureError::NotFound)
    }

    /// Erase `user_id` in one database transaction. Calling again on an erased user
    /// sweeps content written into erased workspaces after the original commit and
    /// reports `already_erased`. S3 objects are not deleted in v1 (spec §10.3).
    pub async fn erase(
        &self,
        user_id: Uuid,
        admin_user_id: Uuid,
        confirm_email: &str,
        requested_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ErasureResult, UserErasureError> {
        let outcome = self
            .repository
            .execute(ExecuteRequest {
                user_id,
                admin_user_id,
                confirm_email,
                requested_at: requested_at.unwrap_or_else(chrono::Utc::now),
            })
            .await
            .map_err(UserErasureError::Internal)?;

        let (already_erased, footprint) = match outcome {
            ExecuteOutcome::Erased(footprint) => {
                tracing::info!(
                    %user_id,
                    %admin_user_id,
                    erased_organizations = footprint.organization_ids.len(),
                    "User erased"
                );
                (false, footprint)
            }
            ExecuteOutcome::AlreadyErased => (
                true,
                self.repository
                    .sweep_erased(user_id)
                    .await
                    .map_err(UserErasureError::Internal)?,
            ),
            ExecuteOutcome::Blocked(blockers) => return Err(UserErasureError::Blocked(blockers)),
            ExecuteOutcome::ConfirmEmailMismatch => {
                return Err(UserErasureError::ConfirmEmailMismatch)
            }
            ExecuteOutcome::NotFound => return Err(UserErasureError::NotFound),
        };

        Ok(ErasureResult {
            user_id,
            already_erased,
            erased_organization_ids: footprint.organization_ids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct FakeRepo {
        outcome: Mutex<ExecuteOutcome>,
        footprint: ErasedFootprint,
        sweeps: Mutex<u32>,
    }

    impl FakeRepo {
        fn new(outcome: ExecuteOutcome, footprint: ErasedFootprint) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(outcome),
                footprint,
                sweeps: Mutex::new(0),
            })
        }
    }

    #[async_trait]
    impl UserErasureRepository for FakeRepo {
        async fn plan(&self, _: Uuid) -> anyhow::Result<Option<ErasurePlan>> {
            Ok(None)
        }
        async fn execute(&self, _: ExecuteRequest<'_>) -> anyhow::Result<ExecuteOutcome> {
            Ok(self.outcome.lock().unwrap().clone())
        }
        async fn sweep_erased(&self, _: Uuid) -> anyhow::Result<ErasedFootprint> {
            *self.sweeps.lock().unwrap() += 1;
            Ok(self.footprint.clone())
        }
    }

    fn footprint() -> (Uuid, ErasedFootprint) {
        let org = Uuid::new_v4();
        (
            org,
            ErasedFootprint {
                organization_ids: vec![org],
                workspace_ids: vec![Uuid::new_v4()],
            },
        )
    }

    #[tokio::test]
    async fn erase_returns_erased_orgs() {
        let (org, fp) = footprint();
        let repo = FakeRepo::new(ExecuteOutcome::Erased(fp.clone()), fp);
        let r = UserErasureService::new(repo.clone())
            .erase(Uuid::new_v4(), Uuid::new_v4(), "a@b.c", None)
            .await
            .unwrap();
        assert!(!r.already_erased);
        assert_eq!(r.erased_organization_ids, vec![org]);
        assert_eq!(
            *repo.sweeps.lock().unwrap(),
            0,
            "first erase does not sweep"
        );
    }

    #[tokio::test]
    async fn retry_on_erased_user_sweeps_and_reports_already_erased() {
        let (org, fp) = footprint();
        let repo = FakeRepo::new(ExecuteOutcome::AlreadyErased, fp);
        let r = UserErasureService::new(repo.clone())
            .erase(Uuid::new_v4(), Uuid::new_v4(), "ignored", None)
            .await
            .unwrap();
        assert!(r.already_erased);
        assert_eq!(r.erased_organization_ids, vec![org]);
        assert_eq!(*repo.sweeps.lock().unwrap(), 1, "retry sweeps late content");
    }

    #[tokio::test]
    async fn outcomes_map_to_errors() {
        for (outcome, expected) in [
            (ExecuteOutcome::NotFound, "NotFound"),
            (ExecuteOutcome::ConfirmEmailMismatch, "ConfirmEmailMismatch"),
            (
                ExecuteOutcome::Blocked(vec![ErasureBlocker::ActiveAdminTokens { count: 1 }]),
                "Blocked",
            ),
        ] {
            let repo = FakeRepo::new(outcome, ErasedFootprint::default());
            let err = UserErasureService::new(repo)
                .erase(Uuid::new_v4(), Uuid::new_v4(), "a@b.c", None)
                .await
                .unwrap_err();
            assert!(format!("{err:?}").starts_with(expected), "{err:?}");
        }
    }
}
