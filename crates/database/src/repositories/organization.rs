use crate::models::{
    CreateOrganizationRequest as DbCreateOrganizationRequest, Organization as DbOrganization,
    OrganizationMember as DbOrganizationMember, OrganizationRole as DbOrganizationRole,
    UpdateOrganizationRequest as DbUpdateOrganizationRequest,
};
use crate::pool::DbPool;
use crate::repositories::utils::map_db_error;
use crate::retry_db;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::auth::ports::UserId;
use services::common::RepositoryError;
use services::organization::ports::*;
use tokio_postgres::types::Type;
use tracing::debug;
use uuid::Uuid;

pub struct PgOrganizationRepository {
    pool: DbPool,
}

impl PgOrganizationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Get the owner of an organization by looking up the owner role in organization_members
    async fn get_organization_owner(&self, org_id: Uuid) -> Result<Option<Uuid>> {
        let row = retry_db!("get_organization_owner", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT user_id FROM organization_members WHERE organization_id = $1 AND role = 'owner'",
                    &[&org_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.map(|r| r.get("user_id")))
    }

    /// Convert database Organization to domain Organization
    async fn db_to_domain_organization(&self, db_org: DbOrganization) -> Result<Organization> {
        let owner_id = self
            .get_organization_owner(db_org.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Organization has no owner: {}", db_org.id))?;

        self.db_to_domain_organization_with_owner(db_org, owner_id)
    }

    fn db_to_domain_organization_with_owner(
        &self,
        db_org: DbOrganization,
        owner_id: Uuid,
    ) -> Result<Organization> {
        Ok(Organization {
            request_priority: db_org.request_priority,
            id: OrganizationId::from(db_org.id),
            name: db_org.name,
            description: db_org.description,
            owner_id: UserId::from(owner_id),
            settings: db_org.settings.unwrap_or_default(),
            is_active: db_org.is_active,
            created_at: db_org.created_at,
            updated_at: db_org.updated_at,
        })
    }

    /// Convert database OrganizationMember to domain OrganizationMember
    fn db_to_domain_member(&self, db_member: DbOrganizationMember) -> Result<OrganizationMember> {
        let role = match db_member.role {
            DbOrganizationRole::Owner => MemberRole::Owner,
            DbOrganizationRole::Admin => MemberRole::Admin,
            DbOrganizationRole::Member => MemberRole::Member,
        };

        Ok(OrganizationMember {
            organization_id: OrganizationId::from(db_member.organization_id),
            user_id: UserId::from(db_member.user_id),
            role,
            joined_at: db_member.joined_at,
        })
    }

    /// Create a new organization and add creator as owner - internal method
    async fn create_internal(
        &self,
        request: DbCreateOrganizationRequest,
        creator_user_id: Uuid,
    ) -> Result<DbOrganization, RepositoryError> {
        let id = Uuid::new_v4();

        let row = retry_db!("create_organization_with_owner", {
            let now = Utc::now();
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client
                .transaction()
                .await
                .context("Failed to start transaction")
                .map_err(RepositoryError::DatabaseError)?;

            // Create the organization
            let row = transaction
                .query_one(
                    r#"
            INSERT INTO organizations (
                id, name, description,
                created_at, updated_at, is_active
            )
            VALUES ($1, $2, $3, $4, $5, true)
            RETURNING *
            "#,
                    &[&id, &request.name, &request.description, &now, &now],
                )
                .await
                .map_err(map_db_error)?;

            // Add creator as owner
            transaction
                .execute(
                    r#"
            INSERT INTO organization_members (organization_id, user_id, role, joined_at)
            VALUES ($1, $2, 'owner', $3)
            "#,
                    &[&id, &creator_user_id, &now],
                )
                .await
                .map_err(map_db_error)?;

            transaction.commit().await.map_err(map_db_error)?;

            Ok(row)
        })?;

        debug!(
            "Created organization: {} with owner: {}",
            id, creator_user_id
        );
        self.row_to_db_organization(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Get an organization by ID - internal method
    async fn get_by_id_internal(
        &self,
        id: Uuid,
    ) -> Result<Option<DbOrganization>, RepositoryError> {
        let row = retry_db!("get_organization_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM organizations WHERE id = $1 AND is_active = true",
                    &[&id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(
                self.row_to_db_organization(row)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    /// Get an organization by name - internal method
    async fn get_by_name_internal(
        &self,
        name: &str,
    ) -> Result<Option<DbOrganization>, RepositoryError> {
        let row = retry_db!("get_organization_by_name", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    "SELECT * FROM organizations WHERE name = $1 AND is_active = true",
                    &[&name],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(
                self.row_to_db_organization(row)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    /// Get organization member by user ID - internal method
    async fn get_member_internal(
        &self,
        organization_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<DbOrganizationMember>, RepositoryError> {
        let row = retry_db!("get_organization_member_by_id", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_opt(
                    r#"
            SELECT * FROM organization_members 
            WHERE organization_id = $1 AND user_id = $2
            "#,
                    &[&organization_id, &user_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        match row {
            Some(row) => Ok(Some(
                self.row_to_db_org_member(row)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    /// Update an organization - internal method
    async fn update_internal(
        &self,
        id: Uuid,
        request: DbUpdateOrganizationRequest,
        expected_fallback_override: Option<Option<serde_json::Value>>,
        actor_user_id: Uuid,
    ) -> Result<DbOrganization, RepositoryError> {
        let row = retry_db!("update_organization", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;
            let locked_organization = transaction
                .query_opt(
                    "SELECT settings FROM organizations
                     WHERE id = $1 AND is_active = true
                     FOR UPDATE",
                    &[&id],
                )
                .await
                .map_err(map_db_error)?
                .ok_or_else(|| RepositoryError::NotFound(id.to_string()))?;
            let current_settings = locked_organization
                .get::<_, Option<serde_json::Value>>("settings")
                .unwrap_or_else(|| serde_json::json!({}));

            let actor_role = transaction
                .query_opt(
                    "SELECT role FROM organization_members
                     WHERE organization_id = $1 AND user_id = $2",
                    &[&id, &actor_user_id],
                )
                .await
                .map_err(map_db_error)?
                .map(|row| row.get::<_, String>("role"));
            if !matches!(actor_role.as_deref(), Some("owner") | Some("admin")) {
                return Err(RepositoryError::ValidationFailed(
                    "Only owners and admins can update organization".to_string(),
                ));
            }

            if let Some(expected) = expected_fallback_override.as_ref() {
                if current_settings.get("fallback_enabled") != expected.as_ref() {
                    return Err(RepositoryError::TransactionConflict);
                }
            }

            let fallback_changed = request.settings.as_ref().is_some_and(|settings| {
                current_settings.get("fallback_enabled") != settings.get("fallback_enabled")
            });
            if fallback_changed && actor_role.as_deref() != Some("owner") {
                return Err(RepositoryError::ValidationFailed(
                    "Only organization owners can manage fallback".to_string(),
                ));
            }

            let row = transaction
                .query_opt(
                    r#"
            UPDATE organizations
            SET name = COALESCE($2, name),
                description = COALESCE($3, description),
                rate_limit = COALESCE($4, rate_limit),
                settings = COALESCE($5, settings),
                updated_at = NOW()
            WHERE id = $1
              AND is_active = true
            RETURNING *
            "#,
                    &[
                        &id,
                        &request.name,
                        &request.description,
                        &request.rate_limit,
                        &request.settings,
                    ],
                )
                .await
                .map_err(map_db_error)?
                .ok_or(RepositoryError::TransactionConflict)?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok(row)
        })?;

        debug!("Updated organization: {}", id);
        self.row_to_db_organization(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Remove a member from an organization
    pub async fn remove_member(&self, org_id: Uuid, user_id: Uuid) -> Result<bool> {
        let rows_affected = retry_db!("remove_organization_member", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .execute(
                    "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                    &[&org_id, &user_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(rows_affected > 0)
    }

    /// Get member count for an organization
    pub async fn get_member_count(&self, org_id: Uuid) -> Result<i64> {
        let row = retry_db!("get_organization_member_count", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    "SELECT COUNT(*) as count FROM organization_members WHERE organization_id = $1",
                    &[&org_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.get("count"))
    }

    // Helper function to convert database row to Organization
    fn row_to_db_organization(&self, row: tokio_postgres::Row) -> anyhow::Result<DbOrganization> {
        Ok(DbOrganization {
            request_priority: row.try_get("request_priority")?,
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            description: row.try_get("description")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            is_active: row.try_get("is_active")?,
            rate_limit: row.try_get("rate_limit")?,
            settings: row.try_get("settings")?,
        })
    }

    // Helper function to convert database row to OrganizationMember
    fn row_to_db_org_member(&self, row: tokio_postgres::Row) -> Result<DbOrganizationMember> {
        let role_str: String = row.get("role");
        let role = match role_str.as_str() {
            "owner" => DbOrganizationRole::Owner,
            "admin" => DbOrganizationRole::Admin,
            "member" => DbOrganizationRole::Member,
            _ => bail!("Invalid role: {role_str}"),
        };

        Ok(DbOrganizationMember {
            id: row.get("id"),
            organization_id: row.get("organization_id"),
            user_id: row.get("user_id"),
            role,
            joined_at: row.get("joined_at"),
            invited_by: row.get("invited_by"),
        })
    }

    /// Count organizations that a user is a member of
    pub async fn count_organizations_by_user(&self, user_id: Uuid) -> Result<i64> {
        let row = retry_db!("count_orgs_by_user", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            client
                .query_one(
                    r#"
                    SELECT COUNT(DISTINCT o.id) as count
                    FROM organizations o
                    INNER JOIN organization_members om ON o.id = om.organization_id
                    WHERE om.user_id = $1 AND o.is_active = true
                    "#,
                    &[&user_id],
                )
                .await
                .map_err(map_db_error)
        })?;

        Ok(row.get::<_, i64>("count"))
    }
}

#[async_trait]
impl OrganizationRepository for PgOrganizationRepository {
    async fn create(
        &self,
        request: CreateOrganizationRequest,
        creator_user_id: Uuid,
    ) -> Result<Organization, RepositoryError> {
        let db_request = DbCreateOrganizationRequest {
            name: request.name.clone(),
            description: request.description,
        };

        let db_org = self.create_internal(db_request, creator_user_id).await?;
        self.db_to_domain_organization(db_org)
            .await
            .map_err(RepositoryError::DataConversionError)
    }

    async fn get_by_id(&self, id: Uuid) -> Result<Option<Organization>, RepositoryError> {
        match self.get_by_id_internal(id).await? {
            Some(db_org) => Ok(Some(
                self.db_to_domain_organization(db_org)
                    .await
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    async fn get_by_name(&self, name: &str) -> Result<Option<Organization>, RepositoryError> {
        match self.get_by_name_internal(name).await? {
            Some(db_org) => Ok(Some(
                self.db_to_domain_organization(db_org)
                    .await
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    async fn get_member(
        &self,
        organization_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<OrganizationMember>, RepositoryError> {
        match self.get_member_internal(organization_id, user_id).await? {
            Some(db_member) => Ok(Some(
                self.db_to_domain_member(db_member)
                    .map_err(RepositoryError::DataConversionError)?,
            )),
            None => Ok(None),
        }
    }

    async fn update(
        &self,
        id: Uuid,
        request: UpdateOrganizationRequest,
        expected_fallback_override: Option<Option<serde_json::Value>>,
        actor_user_id: Uuid,
    ) -> Result<Organization, RepositoryError> {
        let db_request = DbUpdateOrganizationRequest {
            name: request.name,
            description: request.description,
            rate_limit: request.rate_limit,
            settings: request.settings,
        };

        let db_org = self
            .update_internal(id, db_request, expected_fallback_override, actor_user_id)
            .await?;
        self.db_to_domain_organization(db_org)
            .await
            .map_err(RepositoryError::DataConversionError)
    }

    async fn remove_member(
        &self,
        org_id: Uuid,
        user_id: Uuid,
        requester_user_id: Uuid,
    ) -> Result<RemoveOrganizationMemberResult, RepositoryError> {
        retry_db!("remove_member", {
            let mut client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            let transaction = client.transaction().await.map_err(map_db_error)?;

            let organization = transaction
                .query_opt(
                    "SELECT id FROM organizations
                     WHERE id = $1 AND is_active = true
                     FOR UPDATE",
                    &[&org_id],
                )
                .await
                .map_err(map_db_error)?;
            if organization.is_none() {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(RemoveOrganizationMemberResult::NotFound);
            }

            if requester_user_id != user_id {
                let requester_role = transaction
                    .query_opt(
                        "SELECT role FROM organization_members
                         WHERE organization_id = $1 AND user_id = $2",
                        &[&org_id, &requester_user_id],
                    )
                    .await
                    .map_err(map_db_error)?
                    .map(|row| row.get::<_, String>("role"));
                if !matches!(requester_role.as_deref(), Some("owner" | "admin")) {
                    transaction.rollback().await.map_err(map_db_error)?;
                    return Ok(RemoveOrganizationMemberResult::Unauthorized);
                }
            }

            let member = transaction
                .query_opt(
                    "SELECT role FROM organization_members
                     WHERE organization_id = $1 AND user_id = $2
                     FOR UPDATE",
                    &[&org_id, &user_id],
                )
                .await
                .map_err(map_db_error)?;
            let Some(member) = member else {
                transaction.rollback().await.map_err(map_db_error)?;
                return Ok(RemoveOrganizationMemberResult::NotFound);
            };

            let is_bound_wallet: bool = transaction
                .query_one(
                    "SELECT EXISTS (
                        SELECT 1 FROM organization_staking_farm_sources s
                        JOIN users u ON u.auth_provider = 'near'
                          AND u.provider_user_id = s.near_account_id
                        WHERE s.organization_id = $1 AND u.id = $2
                    )",
                    &[&org_id, &user_id],
                )
                .await
                .map_err(map_db_error)?
                .get(0);
            if is_bound_wallet {
                transaction.rollback().await.map_err(map_db_error)?;
                return Err(RepositoryError::ValidationFailed(
                    "The permanently bound NEAR wallet cannot be removed from this organization"
                        .to_string(),
                ));
            }

            if member.get::<_, String>("role") == "owner" {
                let owner_count = transaction
                    .query_one(
                        "SELECT COUNT(*) FROM organization_members
                         WHERE organization_id = $1 AND role = 'owner'",
                        &[&org_id],
                    )
                    .await
                    .map_err(map_db_error)?
                    .get::<_, i64>(0);
                if owner_count <= 1 {
                    transaction.rollback().await.map_err(map_db_error)?;
                    return Ok(RemoveOrganizationMemberResult::LastOwner);
                }
            }

            let rows_affected = transaction
                .execute(
                    "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                    &[&org_id, &user_id],
                )
                .await
                .map_err(map_db_error)?;
            transaction.commit().await.map_err(map_db_error)?;
            Ok(if rows_affected > 0 {
                RemoveOrganizationMemberResult::Removed
            } else {
                RemoveOrganizationMemberResult::NotFound
            })
        })
    }
}

// Implementation of OrganizationConcurrentLimitRepository for completions service
#[async_trait]
impl services::completions::ports::OrganizationConcurrentLimitRepository
    for PgOrganizationRepository
{
    async fn get_concurrent_limit(&self, org_id: Uuid) -> Result<Option<u32>> {
        let row = retry_db!("get_organization_concurrent_limit", {
            let client = self
                .pool
                .get()
                .await
                .context("Failed to get database connection")
                .map_err(RepositoryError::PoolError)?;

            // One-shot typed query: one round trip, see repositories/mod.rs.
            client
                .query_typed_opt(
                    "SELECT rate_limit FROM organizations WHERE id = $1 AND is_active = true",
                    &[(&org_id, Type::UUID)],
                )
                .await
                .map_err(map_db_error)
        })?;

        // Use try_from for safe i32 -> u32 conversion
        // Negative values will become None, falling back to default limit
        Ok(row.and_then(|r| {
            r.get::<_, Option<i32>>("rate_limit")
                .and_then(|v| u32::try_from(v).ok())
        }))
    }
}
