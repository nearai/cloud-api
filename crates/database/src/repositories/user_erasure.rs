use crate::pool::DbPool;
use crate::repositories::lifecycle::ORG_ALL_MEMBERS_ERASED_SQL;
use crate::repositories::organization::deactivate_organization_children;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{Context, Result};
use async_trait::async_trait;
use services::common::RepositoryError;
use services::lifecycle::{OrganizationLifecycle, UserLifecycle};
use services::user_erasure::ports::*;
use tokio_postgres::GenericClient;
use uuid::Uuid;

/// Upper bound on rows returned by an erasure lookup.
const MAX_ERASURE_LOOKUP_RESULTS: i64 = 100;

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

/// Delete user content in the given workspaces. Conversations first: responses and
/// response_items cascade from them; then any responses without a conversation; then files.
async fn delete_workspace_content<C: GenericClient + Sync>(
    client: &C,
    workspace_ids: &[Uuid],
) -> Result<(), RepositoryError> {
    for sql in [
        "DELETE FROM conversations WHERE workspace_id = ANY($1)",
        "DELETE FROM responses WHERE workspace_id = ANY($1)",
        "DELETE FROM files WHERE workspace_id = ANY($1)",
    ] {
        client
            .execute(sql, &[&workspace_ids])
            .await
            .map_err(map_db_error)?;
    }
    Ok(())
}

/// Rule A, over `o` (organizations) with `$1` = user id: the exact auto-generated
/// signup name (`<local>-org-xxxx`) with a default workspace the user created that
/// embeds that name. Identified structurally, since the user's email may have changed
/// since signup. `org-<uuid>` (already renamed) is excluded explicitly because a short
/// local part (for example `org` or a hex run) can occur inside a uuid.
const SIGNUP_ORG_NAME_SQL: &str = r#"
    o.name <> 'org-' || o.id::text
    AND o.name ~ '^.+-org-[a-z0-9]{4}$'
    AND EXISTS (
        SELECT 1 FROM workspaces sw
        WHERE sw.organization_id = o.id
          AND sw.created_by_user_id = $1
          AND sw.description = 'Default workspace for ' || o.name
    )
"#;

/// Rule B, over `o` with `$2` = regex-escaped, lowercased email local part (NULL skips
/// the rule): the lowercased name contains the local part as a whole token, bounded on
/// each side by the start or end of the name or a character outside `[a-z0-9]`. So
/// `dev` matches `dev-team` and `team-dev` but not `devops-team`. A regex over an
/// escaped bind, not LIKE: a local part may contain `_`, `%` or `.`. The minimum length
/// is enforced in Rust (`rename_name_pattern`), which passes NULL for short parts.
const NAME_CONTAINS_LOCAL_SQL: &str = r#"
    o.name <> 'org-' || o.id::text
    AND $2::text IS NOT NULL
    AND lower(o.name) ~ ('(^|[^a-z0-9])' || $2::text || '($|[^a-z0-9])')
"#;

/// Shortest email local part rule B will use; shorter ones would match too much.
const MIN_LOCAL_PART_CHARS: usize = 3;

/// Escapes `s` for use as a literal inside a POSIX regex.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if ".^$*+?()[]{}|\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The part of `email` before the last `@`, trimmed, lowercased and regex-escaped, when
/// it is long enough to match on. Personal data: never log it.
fn rename_name_pattern(email: &str) -> Option<String> {
    let (local, _) = email.trim().rsplit_once('@')?;
    let local = local.trim().to_lowercase();
    (local.chars().count() >= MIN_LOCAL_PART_CHARS).then(|| regex_escape(&local))
}

/// Retained orgs recorded in `user_erasure_log` for user `$1`.
const LOGGED_RETAINED_ORGS_SQL: &str = r#"
    o.id IN (SELECT unnest(l.retained_organization_ids)
             FROM user_erasure_log l WHERE l.user_id = $1)
"#;

/// Post-commit rename predicate over `o`, with `$1` = user id and `$2` = watch org ids:
/// (a) ACTIVE orgs with the signup name (rule A); (b) DELETED orgs this user's erasure
/// log retained, with the signup name (rule A, no email needed, so it repairs on retry);
/// (c) DELETED orgs from the watch list, which matched rule B while active at erase time
/// and were deleted concurrently. `org-<uuid>` is never renamed again.
fn post_commit_rename_predicate() -> String {
    format!(
        "((o.is_active AND ({SIGNUP_ORG_NAME_SQL})) \
         OR (NOT o.is_active AND ({LOGGED_RETAINED_ORGS_SQL}) AND ({SIGNUP_ORG_NAME_SQL})) \
         OR (NOT o.is_active AND o.id = ANY($2) AND o.name <> 'org-' || o.id::text))"
    )
}

/// Default workspaces U created in retained orgs whose description still embeds a signup
/// name the org no longer has (the owner renamed it after signup). `o` = organizations,
/// `w` = workspaces, `$1` = user id. Only the description changes.
const DRIFTED_DESCRIPTION_SQL: &str = r#"
    w.organization_id = o.id
    AND w.created_by_user_id = $1
    AND w.description ~ '^Default workspace for .+-org-[a-z0-9]{4}$'
    AND w.description <> 'Default workspace for ' || o.name
"#;

/// Applies a rename to the `renamed(id, old_name)` CTE that precedes it: org name to
/// `org-<uuid>`, plus the default-workspace description and the denormalised
/// `organization_name` in both email delivery tables where they equal the old name.
const RENAME_UPDATES_SQL: &str = r#"
    , ws AS (
        UPDATE workspaces w
        SET description = 'Default workspace for org-' || r.id::text, updated_at = NOW()
        FROM renamed r
        WHERE w.organization_id = r.id AND w.description = 'Default workspace for ' || r.old_name
    ), dep AS (
        UPDATE model_deprecation_email_deliveries d SET organization_name = 'org-' || r.id::text
        FROM renamed r WHERE d.organization_id = r.id AND d.organization_name = r.old_name
    ), pri AS (
        UPDATE model_pricing_change_email_deliveries d SET organization_name = 'org-' || r.id::text
        FROM renamed r WHERE d.organization_id = r.id AND d.organization_name = r.old_name
    )
    UPDATE organizations o SET name = 'org-' || r.id::text, updated_at = NOW()
    FROM renamed r WHERE o.id = r.id
"#;

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
                retained_organizations: {
                    let retained = retained_org_ids(&memberships);
                    memberships
                        .iter()
                        .filter(|m| retained.contains(&m.organization_id))
                        .map(|m| RetainedOrgSummary {
                            id: m.organization_id,
                            role: m.role.clone(),
                        })
                        .collect()
                },
                log,
            }))
        })?;
        Ok(plan)
    }

    async fn execute(&self, request: ExecuteRequest<'_>) -> Result<ExecuteOutcome> {
        let user_id = request.user_id;
        let confirm = request.confirm_email.trim().to_lowercase();
        let outcome = retry_db!("execute_user_erasure", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;

            let Some(user) = transaction
                .query_opt(
                    "SELECT email, is_active, auth_provider FROM users WHERE id = $1 FOR UPDATE",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?
            else {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ExecuteOutcome::NotFound);
            };
            let lifecycle = UserLifecycle::from_columns(
                user.get("is_active"),
                user.get::<_, &str>("auth_provider"),
            )
            .map_err(|e| RepositoryError::DataConversionError(e.into()))?;
            if lifecycle == UserLifecycle::Erased {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ExecuteOutcome::AlreadyErased);
            }
            let user_email: String = user.get("email");
            if user_email.to_lowercase() != confirm {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ExecuteOutcome::ConfirmEmailMismatch);
            }

            // Lock order matches organization.rs (org row, then member row): read U's
            // memberships unlocked to find the candidate erased orgs, lock those org
            // rows, then lock U's member rows. Retained team orgs are never row-locked
            // here: usage recording (the record_organization_usage SQL function,
            // V0088) takes FOR UPDATE on the org row. The rename of retained ACTIVE orgs
            // runs after commit in its own short org-first transaction
            // (rename_retained_org_names).
            let first = load_memberships(&*transaction, user_id).await?;
            let erased = erased_org_ids(&first);
            transaction
                .query(
                    "SELECT id FROM organizations WHERE id = ANY($1) ORDER BY id FOR UPDATE",
                    &[&erased],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .query(
                    "SELECT 1 FROM organization_members WHERE user_id = $1 ORDER BY organization_id FOR UPDATE",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?;
            // add-member takes FOR SHARE on the org row; re-read under the locks and
            // retry if someone joined or left an org we were about to erase.
            let memberships = load_memberships(&*transaction, user_id).await?;
            if erased_org_ids(&memberships) != erased {
                transaction.rollback().await.map_err(map_db_error)?;
                return Err(RepositoryError::TransactionConflict);
            }
            let blockers = load_blockers(&*transaction, user_id, &memberships).await?;
            if !blockers.is_empty() {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(ExecuteOutcome::Blocked(blockers));
            }
            let retained = retained_org_ids(&memberships);
            let workspace_ids: Vec<Uuid> = transaction
                .query(
                    "SELECT id FROM workspaces WHERE organization_id = ANY($1) ORDER BY id",
                    &[&erased],
                )
                .await
                .map_err(map_db_error)?
                .iter()
                .map(|r| r.get("id"))
                .collect();
            let tombstone = format!("erased+{user_id}@erased.invalid");

            // Deletes.
            delete_workspace_content(&*transaction, &workspace_ids).await?;
            transaction
                .execute(
                    "DELETE FROM mcp_connectors WHERE organization_id = ANY($1)",
                    &[&erased],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "DELETE FROM organization_invitations WHERE organization_id = ANY($1) OR lower(email) = lower($2)",
                    &[&erased, &user_email],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "DELETE FROM organization_members WHERE user_id = $1 AND organization_id = ANY($2)",
                    &[&user_id, &retained],
                )
                .await
                .map_err(map_db_error)?;
            // Deleted retained orgs that still carry U's identity in their name. This
            // runs inside the erase transaction so it is all-or-nothing with the erasure
            // and needs no retry repair. Locking the org rows after U's member rows
            // (the inverse of organization.rs) cannot form a cycle here: these orgs are
            // inactive, so they serve no new requests; the API refuses member and role
            // writes on inactive orgs; and record_organization_usage only takes the org
            // row lock, never member rows. Active orgs are renamed after commit instead.
            // The local part comes from the stored email, not the request.
            let name_pattern = rename_name_pattern(&user_email);
            // One scan finds both: deleted retained orgs to rename now, and still-active
            // ones whose name matches rule B, which are only watched (never locked or
            // renamed here) in case they are deleted before this transaction commits.
            let mut deleted_candidates: Vec<Uuid> = Vec::new();
            let mut rename_watch_org_ids: Vec<Uuid> = Vec::new();
            for row in transaction
                .query(
                    &format!(
                        "SELECT o.id, o.is_active FROM organizations o \
                         WHERE o.id = ANY($3) \
                           AND ((NOT o.is_active AND (({SIGNUP_ORG_NAME_SQL}) OR ({NAME_CONTAINS_LOCAL_SQL}))) \
                                OR (o.is_active AND ({NAME_CONTAINS_LOCAL_SQL}))) \
                         ORDER BY o.id"
                    ),
                    &[&user_id, &name_pattern, &retained],
                )
                .await
                .map_err(map_db_error)?
                .iter()
            {
                if row.get::<_, bool>("is_active") {
                    rename_watch_org_ids.push(row.get("id"));
                } else {
                    deleted_candidates.push(row.get("id"));
                }
            }
            if !deleted_candidates.is_empty() {
                transaction
                    .query(
                        "SELECT id FROM organizations WHERE id = ANY($1) ORDER BY id FOR UPDATE",
                        &[&deleted_candidates],
                    )
                    .await
                    .map_err(map_db_error)?;
                // Re-check the predicate now that the rows are locked.
                transaction
                    .execute(
                        &format!(
                            "WITH renamed AS (\
                                 SELECT o.id, o.name AS old_name FROM organizations o \
                                 WHERE o.id = ANY($3) AND NOT o.is_active \
                                   AND (({SIGNUP_ORG_NAME_SQL}) OR ({NAME_CONTAINS_LOCAL_SQL}))\
                             ){RENAME_UPDATES_SQL}"
                        ),
                        &[&user_id, &name_pattern, &deleted_candidates],
                    )
                    .await
                    .map_err(map_db_error)?;
            }
            for sql in [
                "DELETE FROM feature_request_votes WHERE user_id = $1",
                "DELETE FROM mcp_connector_usage WHERE user_id = $1",
                "DELETE FROM refresh_tokens WHERE user_id = $1",
            ] {
                transaction
                    .execute(sql, &[&user_id])
                    .await
                    .map_err(map_db_error)?;
            }

            // Scrubs: erased orgs and everything under them.
            for org in &erased {
                deactivate_organization_children(&*transaction, *org).await?;
            }
            transaction
                .execute(
                    "UPDATE api_keys SET name = 'erased', key_prefix = 'sk-****' WHERE workspace_id = ANY($1)",
                    &[&workspace_ids],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "UPDATE organization_reporting_tokens SET name = 'erased' WHERE organization_id = ANY($1)",
                    &[&erased],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "UPDATE workspaces SET name = 'erased-' || id::text, description = NULL, settings = NULL, \
                     is_active = false, updated_at = NOW() WHERE id = ANY($1)",
                    &[&workspace_ids],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "UPDATE organizations SET name = 'erased-' || id::text, description = NULL, settings = NULL, \
                     is_active = false, updated_at = NOW() WHERE id = ANY($1)",
                    &[&erased],
                )
                .await
                .map_err(map_db_error)?;
            transaction
                .execute(
                    "UPDATE organization_staking_farm_sources SET status = 'disconnected', updated_at = NOW() \
                     WHERE organization_id = ANY($1)",
                    &[&erased],
                )
                .await
                .map_err(map_db_error)?;

            // Denormalized copies of U's email, provider metadata, org names.
            for sql in [
                "UPDATE organization_limits_history SET changed_by_user_email = $2 WHERE changed_by_user_id = $1",
                "UPDATE model_history SET changed_by_user_email = $2 WHERE changed_by_user_id = $1",
                "UPDATE scheduled_model_pricing_changes SET created_by_user_email = $2 WHERE created_by_user_id = $1",
                "UPDATE scheduled_model_pricing_changes SET cancelled_by_user_email = $2 WHERE cancelled_by_user_id = $1",
                "UPDATE model_deprecation_email_deliveries SET recipient_email = $2, email_last_error = NULL, email_message_id = NULL WHERE recipient_user_id = $1",
                "UPDATE model_deprecation_email_deliveries SET initiated_by_user_email = $2 WHERE initiated_by_user_id = $1",
                "UPDATE model_pricing_change_email_deliveries SET recipient_email = $2, email_last_error = NULL, email_message_id = NULL WHERE recipient_user_id = $1",
                "UPDATE model_pricing_change_email_deliveries SET initiated_by_user_email = $2 WHERE initiated_by_user_id = $1",
            ] {
                transaction
                    .execute(sql, &[&user_id, &tombstone])
                    .await
                    .map_err(map_db_error)?;
            }
            for sql in [
                "UPDATE model_deprecation_email_deliveries SET organization_name = 'erased-' || organization_id::text WHERE organization_id = ANY($1)",
                "UPDATE model_pricing_change_email_deliveries SET organization_name = 'erased-' || organization_id::text WHERE organization_id = ANY($1)",
            ] {
                transaction
                    .execute(sql, &[&erased])
                    .await
                    .map_err(map_db_error)?;
            }
            transaction
                .execute(
                    "UPDATE admin_access_token SET name = 'erased', creation_reason = 'erased', \
                     revocation_reason = CASE WHEN revocation_reason IS NULL THEN NULL ELSE 'erased' END, \
                     user_agent = NULL WHERE created_by_user_id = $1",
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?;

            // The user row, then the log row, then commit.
            transaction
                .execute(
                    "UPDATE users SET email = $2, username = 'erased', display_name = NULL, avatar_url = NULL, \
                     auth_provider = $3, provider_user_id = $1::text, last_login_at = NULL, \
                     is_active = false, tokens_revoked_at = NOW(), updated_at = NOW() WHERE id = $1",
                    &[&user_id, &tombstone, &services::lifecycle::ERASED_AUTH_PROVIDER],
                )
                .await
                .map_err(map_db_error)?;
            let erased_count = erased.len() as i32;
            transaction
                .execute(
                    "INSERT INTO user_erasure_log (user_id, admin_user_id, requested_at, erased_organization_count, \
                     email_sha256, erased_organization_ids, retained_organization_ids) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    &[
                        &user_id,
                        &request.admin_user_id,
                        &request.requested_at,
                        &erased_count,
                        &request.email_sha256.as_slice(),
                        &erased,
                        &retained,
                    ],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;
            Ok(ExecuteOutcome::Erased {
                footprint: ErasedFootprint {
                    organization_ids: erased.clone(),
                    workspace_ids,
                },
                rename_watch_org_ids: rename_watch_org_ids.clone(),
            })
        })?;
        Ok(outcome)
    }

    async fn rename_retained_org_names(&self, user_id: Uuid, watch_org_ids: &[Uuid]) -> Result<()> {
        // Own short transaction, org rows locked first (ordered by id), so it never
        // inverts the org-then-member lock order used by organization.rs. Active
        // retained orgs still have teammates, so this must not run inside the erase
        // transaction. Idempotent: a renamed org (`org-<uuid>`) no longer matches.
        // Covers active signup orgs, deleted retained signup orgs (repairable on retry
        // from the erasure log), and watched orgs deleted since the erase scan.
        let predicate = post_commit_rename_predicate();
        let watch: Vec<Uuid> = watch_org_ids.to_vec();
        retry_db!("rename_retained_org_names", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;
            let candidates: Vec<Uuid> = transaction
                .query(
                    &format!("SELECT o.id FROM organizations o WHERE {predicate} ORDER BY o.id"),
                    &[&user_id, &watch],
                )
                .await
                .map_err(map_db_error)?
                .iter()
                .map(|r| r.get("id"))
                .collect();
            // Retained orgs whose default workspace (created by U) still names an old
            // signup name. Only the description is refreshed, never the org's name.
            let drifted: Vec<Uuid> = transaction
                .query(
                    &format!(
                        "SELECT DISTINCT o.id FROM organizations o \
                         JOIN workspaces w ON w.organization_id = o.id \
                         WHERE {LOGGED_RETAINED_ORGS_SQL} AND {DRIFTED_DESCRIPTION_SQL} \
                         ORDER BY o.id"
                    ),
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?
                .iter()
                .map(|r| r.get("id"))
                .collect();
            let mut lock_ids = candidates.clone();
            lock_ids.extend(&drifted);
            lock_ids.sort();
            lock_ids.dedup();
            if lock_ids.is_empty() {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(());
            }
            transaction
                .query(
                    "SELECT id FROM organizations WHERE id = ANY($1) ORDER BY id FOR UPDATE",
                    &[&lock_ids],
                )
                .await
                .map_err(map_db_error)?;
            // Re-check the predicate now that the rows are locked.
            transaction
                .execute(
                    &format!(
                        "WITH renamed AS (\
                             SELECT o.id, o.name AS old_name FROM organizations o \
                             WHERE o.id = ANY($3) AND {predicate}\
                         ){RENAME_UPDATES_SQL}"
                    ),
                    &[&user_id, &watch, &candidates],
                )
                .await
                .map_err(map_db_error)?;
            // After the org rows, the workspaces. Runs after the rename so renamed orgs
            // are already consistent and only genuinely drifted descriptions remain.
            transaction
                .execute(
                    &format!(
                        "UPDATE workspaces w SET description = 'Default workspace for ' || o.name, \
                         updated_at = NOW() FROM organizations o \
                         WHERE o.id = ANY($2) AND {LOGGED_RETAINED_ORGS_SQL} AND {DRIFTED_DESCRIPTION_SQL}"
                    ),
                    &[&user_id, &lock_ids],
                )
                .await
                .map_err(map_db_error)?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok(())
        })?;
        Ok(())
    }

    async fn sweep_erased(&self, user_id: Uuid) -> Result<ErasedFootprint> {
        // No row locks are taken, which is safe: the sweep only deletes content in
        // orgs that are already inactive and fully erased, deletion is idempotent,
        // and a late write landing after the sweep is caught by the next retry.
        let footprint = retry_db!("sweep_erased_user", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;
            let transaction = client.transaction().await.map_err(map_db_error)?;
            let orgs: Vec<Uuid> = transaction
                .query(
                    &format!(
                        "SELECT o.id FROM organizations o \
                         JOIN organization_members m ON m.organization_id = o.id \
                         WHERE m.user_id = $1 AND NOT o.is_active AND {ORG_ALL_MEMBERS_ERASED_SQL} \
                         ORDER BY o.id"
                    ),
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)?
                .iter()
                .map(|r| r.get("id"))
                .collect();
            let workspaces: Vec<Uuid> = transaction
                .query(
                    "SELECT id FROM workspaces WHERE organization_id = ANY($1) ORDER BY id",
                    &[&orgs],
                )
                .await
                .map_err(map_db_error)?
                .iter()
                .map(|r| r.get("id"))
                .collect();
            delete_workspace_content(&*transaction, &workspaces).await?;
            // A login racing the erase can leave a refresh token behind; drop survivors.
            transaction
                .execute("DELETE FROM refresh_tokens WHERE user_id = $1", &[&user_id])
                .await
                .map_err(map_db_error)?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok(ErasedFootprint {
                organization_ids: orgs,
                workspace_ids: workspaces,
            })
        })?;
        Ok(footprint)
    }
    /// Newest first, capped at [`MAX_ERASURE_LOOKUP_RESULTS`] rows: one email can be
    /// re-registered and erased again, but never anywhere near this many times.
    async fn find_erasures(&self, by: ErasureLookup) -> Result<Vec<ErasureRecord>> {
        let records = retry_db!("find_user_erasures", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let select = r#"
                SELECT l.user_id, l.admin_user_id, l.requested_at, l.erased_at,
                       l.erased_organization_ids, l.retained_organization_ids,
                       u.is_active, u.auth_provider
                FROM user_erasure_log l
                JOIN users u ON u.id = l.user_id
            "#;
            let rows = match &by {
                ErasureLookup::EmailDigest(digest) => {
                    let digest: &[u8] = digest;
                    client
                        .query(
                            &format!(
                                "{select} WHERE l.email_sha256 = $1 ORDER BY l.erased_at DESC, l.id DESC LIMIT {MAX_ERASURE_LOOKUP_RESULTS}"
                            ),
                            &[&digest],
                        )
                        .await
                }
                ErasureLookup::UserId(user_id) => {
                    client
                        .query(
                            &format!(
                                "{select} WHERE l.user_id = $1 ORDER BY l.erased_at DESC, l.id DESC LIMIT {MAX_ERASURE_LOOKUP_RESULTS}"
                            ),
                            &[user_id],
                        )
                        .await
                }
            }
            .map_err(map_db_error)?;

            let mut org_ids: Vec<Uuid> = Vec::new();
            for r in &rows {
                org_ids.extend(r.get::<_, Vec<Uuid>>("erased_organization_ids"));
                org_ids.extend(r.get::<_, Vec<Uuid>>("retained_organization_ids"));
            }
            org_ids.sort();
            org_ids.dedup();
            if rows.is_empty() {
                return Ok(Vec::new());
            }

            let org_rows = client
                .query(
                    &format!(
                        "SELECT o.id, o.is_active, {ORG_ALL_MEMBERS_ERASED_SQL} AS all_members_erased \
                         FROM organizations o WHERE o.id = ANY($1)"
                    ),
                    &[&org_ids],
                )
                .await
                .map_err(map_db_error)?;
            let lifecycles: std::collections::HashMap<Uuid, OrganizationLifecycle> = org_rows
                .iter()
                .map(|r| {
                    (
                        r.get("id"),
                        OrganizationLifecycle::from_columns(
                            r.get("is_active"),
                            r.get("all_members_erased"),
                        ),
                    )
                })
                .collect();
            let orgs = |ids: Vec<Uuid>| -> Vec<ErasureRecordOrg> {
                ids.into_iter()
                    .filter_map(|organization_id| {
                        lifecycles
                            .get(&organization_id)
                            .map(|&lifecycle| ErasureRecordOrg {
                                organization_id,
                                lifecycle,
                            })
                    })
                    .collect()
            };

            rows.iter()
                .map(|r| {
                    Ok(ErasureRecord {
                        user_id: r.get("user_id"),
                        user_lifecycle: UserLifecycle::from_columns(
                            r.get("is_active"),
                            r.get::<_, &str>("auth_provider"),
                        )
                        .map_err(|e| RepositoryError::DataConversionError(e.into()))?,
                        admin_user_id: r.get("admin_user_id"),
                        requested_at: r.get("requested_at"),
                        erased_at: r.get("erased_at"),
                        erased_organizations: orgs(r.get("erased_organization_ids")),
                        retained_organizations: orgs(r.get("retained_organization_ids")),
                    })
                })
                .collect::<Result<Vec<_>, RepositoryError>>()
        })?;
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_escape_escapes_every_metacharacter() {
        assert_eq!(
            regex_escape(r".^$*+?()[]{}|\"),
            r"\.\^\$\*\+\?\(\)\[\]\{\}\|\\"
        );
    }

    #[test]
    fn regex_escape_leaves_plain_characters() {
        assert_eq!(regex_escape("a-b_c9%"), "a-b_c9%");
    }

    #[test]
    fn name_pattern_is_lowercased_escaped_and_length_gated() {
        assert_eq!(
            rename_name_pattern("  Mikalai.Pismiankou+x@Example.com "),
            Some(r"mikalai\.pismiankou\+x".to_string())
        );
        assert_eq!(rename_name_pattern("ab@example.com"), None);
        assert_eq!(rename_name_pattern("no-at-sign"), None);
    }
}
