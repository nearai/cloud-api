//! Admin-driven GDPR erasure of a user. An admin first previews the erasure
//! (blockers and footprint), then executes it with a session only. Execute runs in
//! one transaction that deletes content, tombstones identity, and keeps usage
//! ledgers. Calling execute again on an erased user sweeps late-written content.

pub mod ports;

use std::sync::Arc;

use uuid::Uuid;

pub use ports::*;

/// Domain-separation prefix so these digests don't match generic SHA-256(email) lists.
const ERASED_EMAIL_DIGEST_PREFIX: &str = "nearai-user-erasure-v1:";

/// One-way lookup digest of an erased user's email (trimmed, lowercased). Unkeyed by
/// design (v0): it hides the email from casual reads of the erasure log but does not
/// stop someone with the database from confirming a guessed address.
pub fn erased_email_digest(email: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(ERASED_EMAIL_DIGEST_PREFIX.as_bytes());
    hasher.update(email.trim().to_lowercase().as_bytes());
    hasher.finalize().into()
}

#[derive(Debug, thiserror::Error)]
pub enum UserErasureError {
    #[error("user not found")]
    NotFound,
    #[error("erasure blocked")]
    Blocked(Vec<ErasureBlocker>),
    #[error("confirm_email does not match")]
    ConfirmEmailMismatch,
    #[error("lookup needs exactly one of email or user_id")]
    InvalidLookup,
    #[error("user_id is not a valid UUID")]
    InvalidUserId,
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

    /// Find erasure records by the erased person's email or by user id (exactly one).
    /// The email is digested here; the repository never sees it. Newest first. Supplying
    /// both fields is rejected even if the email is blank, and a blank email on its own
    /// is rejected too.
    pub async fn lookup(
        &self,
        email: Option<&str>,
        user_id: Option<Uuid>,
    ) -> Result<Vec<ErasureRecord>, UserErasureError> {
        let by = match (email, user_id) {
            (Some(email), None) if !email.trim().is_empty() => {
                ErasureLookup::EmailDigest(erased_email_digest(email))
            }
            (None, Some(user_id)) => ErasureLookup::UserId(user_id),
            _ => return Err(UserErasureError::InvalidLookup),
        };
        self.repository
            .find_erasures(by)
            .await
            .map_err(UserErasureError::Internal)
    }

    /// Erase `user_id` in one database transaction. Calling again on an erased user
    /// sweeps content written into erased workspaces after the original commit and
    /// reports `already_erased`. File rows are deleted, but
    /// object bytes stay because the buckets are versioned and replicated; purging
    /// them is a follow-up.
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
                email_sha256: erased_email_digest(confirm_email),
                requested_at: requested_at.unwrap_or_else(chrono::Utc::now),
            })
            .await
            .map_err(UserErasureError::Internal)?;

        let (already_erased, footprint) = match outcome {
            ExecuteOutcome::Erased {
                footprint,
                rename_watch_org_ids,
            } => {
                tracing::info!(
                    %user_id,
                    %admin_user_id,
                    erased_organizations = footprint.organization_ids.len(),
                    "User erased"
                );
                // Deleted retained orgs matching the email were renamed inside the erase
                // transaction. Active ones are renamed here, in their own transaction
                // after the commit, with the watch list of orgs that matched the email
                // while active, in case one was deleted before the commit. If this fails,
                // the operator re-posts: the retry reports `already_erased` and re-runs
                // the idempotent rename. A retry has no watch list, so an org deleted
                // concurrently with the erase is repaired only if this step succeeded
                // the first time (the window: a concurrent org deletion during the
                // erase, followed by a post-commit failure).
                self.repository
                    .rename_retained_org_names(user_id, &rename_watch_org_ids)
                    .await
                    .map_err(|e| {
                        tracing::warn!(%user_id, "erasure committed but retained org rename failed; re-post the erase to sweep and retry the idempotent rename");
                        UserErasureError::Internal(e)
                    })?;
                (false, footprint)
            }
            ExecuteOutcome::AlreadyErased => {
                let footprint = self
                    .repository
                    .sweep_erased(user_id)
                    .await
                    .map_err(UserErasureError::Internal)?;
                // Repairs a crash between the erase commit and the post-commit rename
                // (without the watch list; see above).
                self.repository
                    .rename_retained_org_names(user_id, &[])
                    .await
                    .map_err(|e| {
                        tracing::warn!(%user_id, "erasure committed but retained org rename failed; re-post the erase to sweep and retry the idempotent rename");
                        UserErasureError::Internal(e)
                    })?;
                (true, footprint)
            }
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

    #[test]
    fn email_digest_is_deterministic() {
        assert_eq!(
            erased_email_digest("alice@example.com"),
            erased_email_digest("alice@example.com")
        );
    }

    #[test]
    fn email_digest_ignores_case_and_surrounding_whitespace() {
        assert_eq!(
            erased_email_digest("alice@example.com"),
            erased_email_digest("  Alice@Example.COM \n")
        );
    }

    #[test]
    fn email_digest_differs_between_emails() {
        assert_ne!(
            erased_email_digest("alice@example.com"),
            erased_email_digest("bob@example.com")
        );
    }

    #[test]
    fn email_digest_matches_known_vector() {
        // printf 'nearai-user-erasure-v1:alice@example.com' | shasum -a 256
        assert_eq!(
            hex::encode(erased_email_digest("alice@example.com")),
            "8824074f2e471186e93c0a3a804fa8d9259482e007bd24a3e14c4e589043d787"
        );
    }

    struct FakeRepo {
        rename_args: Mutex<Vec<(Uuid, Vec<Uuid>)>>,
        outcome: Mutex<ExecuteOutcome>,
        footprint: ErasedFootprint,
        sweeps: Mutex<u32>,
        renames: Mutex<u32>,
        lookups: Mutex<Vec<ErasureLookup>>,
    }

    impl FakeRepo {
        fn new(outcome: ExecuteOutcome, footprint: ErasedFootprint) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(outcome),
                footprint,
                sweeps: Mutex::new(0),
                renames: Mutex::new(0),
                rename_args: Mutex::new(Vec::new()),
                lookups: Mutex::new(Vec::new()),
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
        async fn rename_retained_org_names(
            &self,
            user_id: Uuid,
            watch_org_ids: &[Uuid],
        ) -> anyhow::Result<()> {
            self.rename_args
                .lock()
                .unwrap()
                .push((user_id, watch_org_ids.to_vec()));
            *self.renames.lock().unwrap() += 1;
            Ok(())
        }
        async fn sweep_erased(&self, _: Uuid) -> anyhow::Result<ErasedFootprint> {
            *self.sweeps.lock().unwrap() += 1;
            Ok(self.footprint.clone())
        }
        async fn find_erasures(&self, by: ErasureLookup) -> anyhow::Result<Vec<ErasureRecord>> {
            self.lookups.lock().unwrap().push(by);
            Ok(Vec::new())
        }
    }

    fn erased(footprint: ErasedFootprint, rename_watch_org_ids: Vec<Uuid>) -> ExecuteOutcome {
        ExecuteOutcome::Erased {
            footprint,
            rename_watch_org_ids,
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
        let repo = FakeRepo::new(erased(fp.clone(), vec![]), fp);
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
        assert_eq!(*repo.renames.lock().unwrap(), 1, "fresh erase renames");
    }

    #[tokio::test]
    async fn fresh_erase_renames_with_the_user_id() {
        let (_, fp) = footprint();
        let repo = FakeRepo::new(erased(fp.clone(), vec![]), fp);
        let user_id = Uuid::new_v4();
        UserErasureService::new(repo.clone())
            .erase(user_id, Uuid::new_v4(), "Alice.B@Example.com", None)
            .await
            .unwrap();
        assert_eq!(*repo.rename_args.lock().unwrap(), vec![(user_id, vec![])]);
    }

    #[tokio::test]
    async fn fresh_erase_passes_the_watch_ids_to_the_rename() {
        let (_, fp) = footprint();
        let watch = vec![Uuid::new_v4(), Uuid::new_v4()];
        let repo = FakeRepo::new(erased(fp.clone(), watch.clone()), fp);
        let user_id = Uuid::new_v4();
        UserErasureService::new(repo.clone())
            .erase(user_id, Uuid::new_v4(), "a@b.c", None)
            .await
            .unwrap();
        assert_eq!(*repo.rename_args.lock().unwrap(), vec![(user_id, watch)]);
    }

    #[tokio::test]
    async fn retry_on_erased_user_sweeps_and_reports_already_erased() {
        let (org, fp) = footprint();
        let repo = FakeRepo::new(ExecuteOutcome::AlreadyErased, fp);
        let user_id = Uuid::new_v4();
        let r = UserErasureService::new(repo.clone())
            .erase(user_id, Uuid::new_v4(), "ignored", None)
            .await
            .unwrap();
        assert!(r.already_erased);
        assert_eq!(r.erased_organization_ids, vec![org]);
        assert_eq!(*repo.sweeps.lock().unwrap(), 1, "retry sweeps late content");
        assert_eq!(*repo.renames.lock().unwrap(), 1, "retry re-runs the rename");
        assert_eq!(
            *repo.rename_args.lock().unwrap(),
            vec![(user_id, vec![])],
            "retry renames with the user id and no watch list"
        );
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

    #[tokio::test]
    async fn lookup_by_email_passes_the_normalized_digest() {
        let repo = FakeRepo::new(ExecuteOutcome::NotFound, ErasedFootprint::default());
        UserErasureService::new(repo.clone())
            .lookup(Some("  Alice@Example.COM "), None)
            .await
            .unwrap();
        assert_eq!(
            *repo.lookups.lock().unwrap(),
            vec![ErasureLookup::EmailDigest(erased_email_digest(
                "alice@example.com"
            ))]
        );
    }

    #[tokio::test]
    async fn lookup_by_user_id_passes_it_through() {
        let repo = FakeRepo::new(ExecuteOutcome::NotFound, ErasedFootprint::default());
        let id = Uuid::new_v4();
        UserErasureService::new(repo.clone())
            .lookup(None, Some(id))
            .await
            .unwrap();
        assert_eq!(
            *repo.lookups.lock().unwrap(),
            vec![ErasureLookup::UserId(id)]
        );
    }

    #[tokio::test]
    async fn lookup_rejects_both_or_neither() {
        for (email, user_id) in [
            (Some("a@b.c"), Some(Uuid::new_v4())),
            (None, None),
            (Some("   "), None),
            (Some("   "), Some(Uuid::new_v4())),
        ] {
            let repo = FakeRepo::new(ExecuteOutcome::NotFound, ErasedFootprint::default());
            let err = UserErasureService::new(repo.clone())
                .lookup(email, user_id)
                .await
                .unwrap_err();
            assert!(matches!(err, UserErasureError::InvalidLookup), "{err:?}");
            assert!(repo.lookups.lock().unwrap().is_empty());
        }
    }
}
