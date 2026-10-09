use crate::lifecycle::{OrganizationLifecycle, UserLifecycle};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    pub requested_at: DateTime<Utc>,
}

#[async_trait]
pub trait UserErasureRepository: Send + Sync {
    /// Read-only. `None` when the user does not exist.
    async fn plan(&self, user_id: Uuid) -> anyhow::Result<Option<ErasurePlan>>;
    /// One transaction: lock, re-check, delete, scrub, write the log row.
    async fn execute(&self, request: ExecuteRequest<'_>) -> anyhow::Result<ExecuteOutcome>;
    /// For an erased user: re-delete content written into erased workspaces after the
    /// original commit, and return the erased orgs and workspaces.
    async fn sweep_erased(&self, user_id: Uuid) -> anyhow::Result<ErasedFootprint>;
}
