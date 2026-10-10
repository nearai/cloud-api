use crate::lifecycle::{OrganizationLifecycle, UserLifecycle};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum ErasureBlocker {
    SoleOwnerOfSharedOrg { organization_id: Uuid },
    StakingActive { organization_id: Uuid },
    ActiveAdminTokens { count: i64 },
}

#[derive(Debug, Clone)]
pub struct ErasureLogEntry {
    pub requested_at: DateTime<Utc>,
    pub erased_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ErasedOrgSummary {
    pub id: Uuid,
    pub lifecycle: OrganizationLifecycle,
    pub workspaces: i64,
    pub api_keys: i64,
    pub conversations: i64,
    pub files: i64,
}

#[derive(Debug, Clone)]
pub struct RetainedOrgSummary {
    pub id: Uuid,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct ErasurePlan {
    pub user_id: Uuid,
    pub lifecycle: UserLifecycle,
    pub blockers: Vec<ErasureBlocker>,
    pub erased_organizations: Vec<ErasedOrgSummary>,
    pub retained_organizations: Vec<RetainedOrgSummary>,
    pub log: Option<ErasureLogEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErasedFootprint {
    pub organization_ids: Vec<Uuid>,
    pub workspace_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecuteOutcome {
    Erased(ErasedFootprint),
    AlreadyErased,
    Blocked(Vec<ErasureBlocker>),
    ConfirmEmailMismatch,
    NotFound,
}

pub struct ExecuteRequest<'a> {
    pub user_id: Uuid,
    pub admin_user_id: Uuid,
    pub confirm_email: &'a str,
    /// `erased_email_digest` of the user's email (execute only proceeds when
    /// `confirm_email` matches the stored email).
    pub email_sha256: [u8; 32],
    pub requested_at: DateTime<Utc>,
}

/// How to find erasure log rows. The repository never sees a raw email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureLookup {
    EmailDigest([u8; 32]),
    UserId(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureRecordOrg {
    pub organization_id: Uuid,
    pub lifecycle: OrganizationLifecycle,
}

/// One `user_erasure_log` row with the current lifecycle of the user and its orgs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureRecord {
    pub user_id: Uuid,
    pub user_lifecycle: UserLifecycle,
    pub admin_user_id: Uuid,
    pub requested_at: DateTime<Utc>,
    pub erased_at: DateTime<Utc>,
    pub erased_organizations: Vec<ErasureRecordOrg>,
    pub retained_organizations: Vec<ErasureRecordOrg>,
}

#[async_trait]
pub trait UserErasureRepository: Send + Sync {
    /// Read-only. `None` when the user does not exist.
    async fn plan(&self, user_id: Uuid) -> anyhow::Result<Option<ErasurePlan>>;
    /// One transaction: lock, re-check, delete, scrub, write the log row.
    async fn execute(&self, request: ExecuteRequest<'_>) -> anyhow::Result<ExecuteOutcome>;
    /// Rename retained orgs that still carry the user's identity in their name to
    /// `org-<uuid>`: auto-named signup orgs (active or deleted), and, when
    /// `email_local` (the user's email local part, at least 3 characters) is given,
    /// deleted orgs the user was retained in whose name contains it. Runs in its own
    /// short org-first transaction, after `execute` commits, because it must row-lock
    /// orgs that still have active teammates. Idempotent. Pass `None` once the email
    /// is tombstoned; the local-part rule is then skipped.
    async fn rename_retained_org_names(
        &self,
        user_id: Uuid,
        email_local: Option<&str>,
    ) -> anyhow::Result<()>;
    /// For an erased user: re-delete content written into erased workspaces after the
    /// original commit, and return the erased orgs and workspaces.
    async fn sweep_erased(&self, user_id: Uuid) -> anyhow::Result<ErasedFootprint>;
    /// Read-only. Erasure log rows matching `by`, newest `erased_at` first. An org id
    /// whose row no longer exists is skipped.
    async fn find_erasures(&self, by: ErasureLookup) -> anyhow::Result<Vec<ErasureRecord>>;
}
