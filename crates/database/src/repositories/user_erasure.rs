use crate::pool::DbPool;
use crate::repositories::lifecycle::ORG_ALL_MEMBERS_ERASED_SQL;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::lifecycle::{OrganizationLifecycle, UserLifecycle};
use services::user_erasure::ports::*;
use tokio_postgres::GenericClient;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct PostgresUserErasureRepository {
    pool: DbPool,
}

impl PostgresUserErasureRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

/// One membership of the target user, with the facts erasure decides on.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Membership {
    pub organization_id: Uuid,
    pub role: String,
    pub org_is_active: bool,
    pub other_members: i64,
    pub other_owners: i64,
}

pub(crate) async fn load_memberships<C: GenericClient + Sync>(
    client: &C,
    user_id: Uuid,
) -> Result<Vec<Membership>, RepositoryError> {
    let rows = client
        .query(
            r#"
            SELECT m.organization_id, m.role, o.is_active AS org_is_active,
                   (SELECT COUNT(*) FROM organization_members x
                     WHERE x.organization_id = m.organization_id AND x.user_id <> $1) AS other_members,
                   (SELECT COUNT(*) FROM organization_members x
                     WHERE x.organization_id = m.organization_id AND x.user_id <> $1
                       AND x.role = 'owner') AS other_owners
            FROM organization_members m
            JOIN organizations o ON o.id = m.organization_id
            WHERE m.user_id = $1
            ORDER BY m.organization_id
            "#,
            &[&user_id],
        )
        .await
        .map_err(map_db_error)?;
    Ok(rows
        .iter()
        .map(|r| Membership {
            organization_id: r.get("organization_id"),
            role: r.get("role"),
            org_is_active: r.get("org_is_active"),
            other_members: r.get("other_members"),
            other_owners: r.get("other_owners"),
        })
        .collect())
}

/// Orgs erased with the user: those where nobody else is a member.
pub(crate) fn erased_org_ids(memberships: &[Membership]) -> Vec<Uuid> {
    memberships
        .iter()
        .filter(|m| m.other_members == 0)
        .map(|m| m.organization_id)
        .collect()
}

/// Orgs the user leaves: anyone else is still a member.
#[allow(dead_code)]
pub(crate) fn retained_org_ids(memberships: &[Membership]) -> Vec<Uuid> {
    memberships
        .iter()
        .filter(|m| m.other_members > 0)
        .map(|m| m.organization_id)
        .collect()
}

pub(crate) async fn load_blockers<C: GenericClient + Sync>(
    client: &C,
    user_id: Uuid,
    memberships: &[Membership],
) -> Result<Vec<ErasureBlocker>, RepositoryError> {
    // Only active orgs: an owner-deleted shared org cannot be transferred (the admin
    // role update requires an active org), and nobody can use it, so U just leaves it.
    let mut blockers: Vec<ErasureBlocker> = memberships
        .iter()
        .filter(|m| {
            m.org_is_active && m.role == "owner" && m.other_members > 0 && m.other_owners == 0
        })
        .map(|m| ErasureBlocker::SoleOwnerOfSharedOrg {
            organization_id: m.organization_id,
        })
        .collect();

    let erased = erased_org_ids(memberships);
    let staking = client
        .query(
            r#"
            SELECT DISTINCT organization_id
            FROM organization_staking_farm_sources
            WHERE status = 'active'
              AND (organization_id = ANY($1) OR created_by_user_id = $2)
              AND (sync_status <> 'synced'
                   OR active_positions <> '[]'::jsonb
                   OR last_synced_at IS NULL
                   OR last_synced_at < NOW() - INTERVAL '24 hours')
            ORDER BY organization_id
            "#,
            &[&erased, &user_id],
        )
        .await
        .map_err(map_db_error)?;
    blockers.extend(staking.iter().map(|r| ErasureBlocker::StakingActive {
        organization_id: r.get("organization_id"),
    }));

    let tokens: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM admin_access_token \
             WHERE created_by_user_id = $1 AND is_active AND revoked_at IS NULL \
               AND expires_at > NOW()",
            &[&user_id],
        )
        .await
        .map_err(map_db_error)?
        .get(0);
    if tokens > 0 {
        blockers.push(ErasureBlocker::ActiveAdminTokens { count: tokens });
    }
    Ok(blockers)
}

#[async_trait]
impl UserErasureRepository for PostgresUserErasureRepository {
    async fn plan(&self, user_id: Uuid) -> Result<Option<ErasurePlan>> {
        let plan = retry_db!("plan_user_erasure", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let Some(user) = client
                .query_opt(
                    "SELECT is_active, auth_provider FROM users WHERE id = $1",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?
            else {
                return Ok(None);
            };
            let lifecycle = UserLifecycle::from_columns(
                user.get("is_active"),
                user.get::<_, &str>("auth_provider"),
            )
            .map_err(|e| RepositoryError::DataConversionError(e.into()))?;

            let memberships = load_memberships(&**client, user_id).await?;
            let blockers = if lifecycle == UserLifecycle::Erased {
                Vec::new()
            } else {
                load_blockers(&**client, user_id, &memberships).await?
            };

            let erased_ids = erased_org_ids(&memberships);
            let summaries = client
                .query(
                    &format!(
                        r#"
                        SELECT o.id, o.is_active, {ORG_ALL_MEMBERS_ERASED_SQL} AS all_members_erased,
                          (SELECT COUNT(*) FROM workspaces w WHERE w.organization_id = o.id) AS workspaces,
                          (SELECT COUNT(*) FROM api_keys k JOIN workspaces w ON w.id = k.workspace_id
                             WHERE w.organization_id = o.id) AS api_keys,
                          (SELECT COUNT(*) FROM conversations c JOIN workspaces w ON w.id = c.workspace_id
                             WHERE w.organization_id = o.id) AS conversations,
                          (SELECT COUNT(*) FROM files f JOIN workspaces w ON w.id = f.workspace_id
                             WHERE w.organization_id = o.id) AS files
                        FROM organizations o
                        WHERE o.id = ANY($1)
                        ORDER BY o.id
                        "#
                    ),
                    &[&erased_ids],
                )
                .await
                .map_err(map_db_error)?;

            let log = client
                .query_opt(
                    "SELECT requested_at, erased_at FROM user_erasure_log WHERE user_id = $1",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?
                .map(|r| ErasureLogEntry {
                    requested_at: r.get("requested_at"),
                    erased_at: r.get("erased_at"),
                });

            Ok(Some(ErasurePlan {
                user_id,
                lifecycle,
                blockers,
                erased_organizations: summaries
                    .iter()
                    .map(|r| ErasedOrgSummary {
                        id: r.get("id"),
                        lifecycle: OrganizationLifecycle::from_columns(
                            r.get("is_active"),
                            r.get("all_members_erased"),
                        ),
                        workspaces: r.get("workspaces"),
                        api_keys: r.get("api_keys"),
                        conversations: r.get("conversations"),
                        files: r.get("files"),
                    })
                    .collect(),
                retained_organizations: memberships
                    .iter()
                    .filter(|m| m.other_members > 0)
                    .map(|m| RetainedOrgSummary {
                        id: m.organization_id,
                        role: m.role.clone(),
                    })
                    .collect(),
                log,
            }))
        })?;
        Ok(plan)
    }

    async fn execute(&self, _request: ExecuteRequest<'_>) -> Result<ExecuteOutcome> {
        anyhow::bail!("user erasure execute is wired in a later task")
    }

    async fn sweep_erased(&self, _user_id: Uuid) -> Result<ErasedFootprint> {
        anyhow::bail!("user erasure sweep is wired in a later task")
    }
}
