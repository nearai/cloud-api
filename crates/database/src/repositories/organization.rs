use crate::models::{
    AddOrganizationMemberRequest as DbAddOrganizationMemberRequest,
    CreateOrganizationRequest as DbCreateOrganizationRequest, Organization as DbOrganization,
    OrganizationMember as DbOrganizationMember, OrganizationRole as DbOrganizationRole,
    UpdateOrganizationMemberRequest as DbUpdateOrganizationMemberRequest,
    UpdateOrganizationRequest as DbUpdateOrganizationRequest,
};
use crate::pool::DbPool;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use services::auth::ports::UserId;
use services::common::RepositoryError;
use services::organization::ports::*;
use tokio_postgres::error::SqlState;
use tracing::debug;
use uuid::Uuid;

pub struct PgOrganizationRepository {
    pool: DbPool,
}

impl PgOrganizationRepository {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Convert tokio_postgres::Error to RepositoryError
    fn map_db_error(err: tokio_postgres::Error) -> RepositoryError {
        // Handle database-level errors (connection, auth, etc.)
        if err.is_closed() {
            return RepositoryError::ConnectionFailed("Connection closed".to_string());
        }

        // Handle SQL state errors
        if let Some(db_err) = err.as_db_error() {
            let message = db_err.message();

            match db_err.code() {
                // Integrity constraint violations
                &SqlState::UNIQUE_VIOLATION => RepositoryError::AlreadyExists,
                &SqlState::FOREIGN_KEY_VIOLATION => {
                    RepositoryError::ForeignKeyViolation(message.to_string())
                }
                &SqlState::NOT_NULL_VIOLATION => {
                    RepositoryError::RequiredFieldMissing(message.to_string())
                }
                &SqlState::CHECK_VIOLATION => {
                    RepositoryError::ValidationFailed(message.to_string())
                }
                &SqlState::RESTRICT_VIOLATION => {
                    RepositoryError::DependencyExists(message.to_string())
                }

                // Transaction errors
                &SqlState::T_R_SERIALIZATION_FAILURE | &SqlState::T_R_DEADLOCK_DETECTED => {
                    RepositoryError::TransactionConflict
                }

                // Connection/auth errors
                &SqlState::INVALID_PASSWORD | &SqlState::INVALID_AUTHORIZATION_SPECIFICATION => {
                    RepositoryError::AuthenticationFailed
                }
                &SqlState::CONNECTION_EXCEPTION
                | &SqlState::CONNECTION_DOES_NOT_EXIST
                | &SqlState::CONNECTION_FAILURE => {
                    RepositoryError::ConnectionFailed(message.to_string())
                }

                // Default case - wrap in generic database error
                _ => RepositoryError::DatabaseError(anyhow::anyhow!(
                    "Database error ({}): {}",
                    db_err.code().code(),
                    message
                )),
            }
        } else {
            // Non-SQL errors (connection issues, etc.)
            RepositoryError::DatabaseError(err.into())
        }
    }

    /// Get the owner of an organization by looking up the owner role in organization_members
    async fn get_organization_owner(&self, org_id: Uuid) -> Result<Option<Uuid>> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_opt(
                "SELECT user_id FROM organization_members WHERE organization_id = $1 AND role = 'owner'",
                &[&org_id],
            )
            .await
            .context("Failed to query organization owner")?;

        Ok(row.map(|r| r.get("user_id")))
    }

    /// Convert database Organization to domain Organization
    async fn db_to_domain_organization(&self, db_org: DbOrganization) -> Result<Organization> {
        let owner_id = self
            .get_organization_owner(db_org.id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Organization has no owner: {}", db_org.id))?;

        Ok(Organization {
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

    /// Convert domain MemberRole to database OrganizationRole  
    fn domain_to_db_role(&self, role: MemberRole) -> DbOrganizationRole {
        match role {
            MemberRole::Owner => DbOrganizationRole::Owner,
            MemberRole::Admin => DbOrganizationRole::Admin,
            MemberRole::Member => DbOrganizationRole::Member,
        }
    }

    /// Create a new organization and add creator as owner - internal method
    async fn create_internal(
        &self,
        request: DbCreateOrganizationRequest,
        creator_user_id: Uuid,
    ) -> Result<DbOrganization, RepositoryError> {
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

        let id = Uuid::new_v4();
        let now = Utc::now();

        // Create the organization
        let row = transaction
            .query_one(
                r#"
            INSERT INTO organizations (
                id, name, display_name, description, 
                created_at, updated_at, is_active
            )
            VALUES ($1, $2, $3, $4, $5, $6, true)
            RETURNING *
            "#,
                &[
                    &id,
                    &request.name,
                    &request.display_name,
                    &request.description,
                    &now,
                    &now,
                ],
            )
            .await
            .map_err(Self::map_db_error)?;

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
            .map_err(Self::map_db_error)?;

        transaction.commit().await.map_err(Self::map_db_error)?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_opt(
                "SELECT * FROM organizations WHERE id = $1 AND is_active = true",
                &[&id],
            )
            .await
            .map_err(Self::map_db_error)?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_opt(
                "SELECT * FROM organizations WHERE name = $1 AND is_active = true",
                &[&name],
            )
            .await
            .map_err(Self::map_db_error)?;

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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_opt(
                r#"
            SELECT * FROM organization_members 
            WHERE organization_id = $1 AND user_id = $2
            "#,
                &[&organization_id, &user_id],
            )
            .await
            .map_err(Self::map_db_error)?;

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
    ) -> Result<DbOrganization, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_one(
                r#"
            UPDATE organizations
            SET display_name = COALESCE($2, display_name),
                description = COALESCE($3, description),
                rate_limit = COALESCE($4, rate_limit),
                settings = COALESCE($5, settings),
                updated_at = NOW()
            WHERE id = $1 AND is_active = true
            RETURNING *
            "#,
                &[
                    &id,
                    &request.display_name,
                    &request.description,
                    &request.rate_limit,
                    &request.settings,
                ],
            )
            .await
            .map_err(Self::map_db_error)?;

        debug!("Updated organization: {}", id);
        self.row_to_db_organization(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Delete an organization (soft delete)
    pub async fn delete(&self, id: Uuid) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows_affected = client
            .execute(
                "UPDATE organizations SET is_active = false WHERE id = $1 AND is_active = true",
                &[&id],
            )
            .await
            .context("Failed to delete organization")?;

        Ok(rows_affected > 0)
    }

    /// Add a member to an organization - internal method
    async fn add_member_internal(
        &self,
        org_id: Uuid,
        request: DbAddOrganizationMemberRequest,
        invited_by: Uuid,
    ) -> Result<DbOrganizationMember, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        // Check if user is already a member
        let existing = client
            .query_opt(
                "SELECT * FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                &[&org_id, &request.user_id],
            )
            .await
            .map_err(Self::map_db_error)?;

        if existing.is_some() {
            return Err(RepositoryError::AlreadyExists);
        }

        let id = Uuid::new_v4();
        let now = Utc::now();

        let row = client.query_one(
            r#"
            INSERT INTO organization_members (id, organization_id, user_id, role, joined_at, invited_by)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING *
            "#,
            &[
                &id,
                &org_id,
                &request.user_id,
                &request.role.to_string().to_lowercase(),
                &now,
                &invited_by,
            ],
        ).await.map_err(Self::map_db_error)?;

        debug!(
            "Added member {} to organization {} with role {:?}",
            request.user_id, org_id, request.role
        );
        self.row_to_db_org_member(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Update a member's role in an organization - internal method
    async fn update_member_internal(
        &self,
        org_id: Uuid,
        user_id: Uuid,
        request: DbUpdateOrganizationMemberRequest,
    ) -> Result<DbOrganizationMember, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_one(
                r#"
            UPDATE organization_members
            SET role = $3
            WHERE organization_id = $1 AND user_id = $2
            RETURNING *
            "#,
                &[&org_id, &user_id, &request.role.to_string().to_lowercase()],
            )
            .await
            .map_err(Self::map_db_error)?;

        debug!(
            "Updated member {} in organization {} to role {:?}",
            user_id, org_id, request.role
        );
        self.row_to_db_org_member(row)
            .map_err(RepositoryError::DataConversionError)
    }

    /// Remove a member from an organization
    pub async fn remove_member(&self, org_id: Uuid, user_id: Uuid) -> Result<bool> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let rows_affected = client
            .execute(
                "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                &[&org_id, &user_id],
            )
            .await
            .context("Failed to remove organization member")?;

        Ok(rows_affected > 0)
    }

    /// List members of an organization with pagination - internal method
    async fn list_members_paginated_internal(
        &self,
        org_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<DbOrganizationMember>, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let rows = client
            .query(
                "SELECT * FROM organization_members WHERE organization_id = $1 ORDER BY joined_at DESC LIMIT $2 OFFSET $3",
                &[&org_id, &limit, &offset],
            )
            .await
            .map_err(Self::map_db_error)?;

        rows.into_iter()
            .map(|row| {
                self.row_to_db_org_member(row)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    /// Get member count for an organization
    pub async fn get_member_count(&self, org_id: Uuid) -> Result<i64> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
            .query_one(
                "SELECT COUNT(*) as count FROM organization_members WHERE organization_id = $1",
                &[&org_id],
            )
            .await
            .context("Failed to count organization members")?;

        Ok(row.get("count"))
    }

    // Helper function to convert database row to Organization
    fn row_to_db_organization(&self, row: tokio_postgres::Row) -> anyhow::Result<DbOrganization> {
        Ok(DbOrganization {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            display_name: row.try_get("display_name")?,
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
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")?;

        let row = client
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
            .context("Failed to count organizations by user")?;

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
            display_name: request.display_name.unwrap_or(request.name),
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
    ) -> Result<Organization, RepositoryError> {
        let db_request = DbUpdateOrganizationRequest {
            display_name: request.display_name,
            description: request.description,
            rate_limit: request.rate_limit,
            settings: request.settings,
        };

        let db_org = self.update_internal(id, db_request).await?;
        self.db_to_domain_organization(db_org)
            .await
            .map_err(RepositoryError::DataConversionError)
    }

    async fn delete(&self, id: Uuid) -> Result<bool, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let rows_affected = client
            .execute(
                "UPDATE organizations SET is_active = false WHERE id = $1 AND is_active = true",
                &[&id],
            )
            .await
            .map_err(Self::map_db_error)?;

        Ok(rows_affected > 0)
    }

    async fn add_member(
        &self,
        org_id: Uuid,
        request: AddOrganizationMemberRequest,
        invited_by: Uuid,
    ) -> Result<OrganizationMember, RepositoryError> {
        let db_request = DbAddOrganizationMemberRequest {
            user_id: request.user_id,
            role: self.domain_to_db_role(request.role),
        };

        let db_member = self
            .add_member_internal(org_id, db_request, invited_by)
            .await?;
        self.db_to_domain_member(db_member)
            .map_err(RepositoryError::DataConversionError)
    }

    async fn update_member(
        &self,
        org_id: Uuid,
        user_id: Uuid,
        request: UpdateOrganizationMemberRequest,
    ) -> Result<OrganizationMember, RepositoryError> {
        let db_request = DbUpdateOrganizationMemberRequest {
            role: self.domain_to_db_role(request.role),
        };

        let db_member = self
            .update_member_internal(org_id, user_id, db_request)
            .await?;
        self.db_to_domain_member(db_member)
            .map_err(RepositoryError::DataConversionError)
    }

    async fn remove_member(&self, org_id: Uuid, user_id: Uuid) -> Result<bool, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let rows_affected = client
            .execute(
                "DELETE FROM organization_members WHERE organization_id = $1 AND user_id = $2",
                &[&org_id, &user_id],
            )
            .await
            .map_err(Self::map_db_error)?;

        Ok(rows_affected > 0)
    }

    async fn list_members_paginated(
        &self,
        org_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<OrganizationMember>, RepositoryError> {
        let db_members = self
            .list_members_paginated_internal(org_id, limit, offset)
            .await?;
        db_members
            .into_iter()
            .map(|db_member| {
                self.db_to_domain_member(db_member)
                    .map_err(RepositoryError::DataConversionError)
            })
            .collect()
    }

    async fn get_member_count(&self, org_id: Uuid) -> Result<i64, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
            .query_one(
                "SELECT COUNT(*) as count FROM organization_members WHERE organization_id = $1",
                &[&org_id],
            )
            .await
            .map_err(Self::map_db_error)?;

        Ok(row.get("count"))
    }

    async fn count_organizations_by_user(&self, user_id: Uuid) -> Result<i64, RepositoryError> {
        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let row = client
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
            .map_err(Self::map_db_error)?;

        Ok(row.get::<_, i64>("count"))
    }

    async fn list_organizations_by_user(
        &self,
        user_id: Uuid,
        limit: i64,
        offset: i64,
        order_by: Option<OrganizationOrderBy>,
        order_direction: Option<OrganizationOrderDirection>,
    ) -> Result<Vec<Organization>, RepositoryError> {
        let order_by = order_by.unwrap_or(OrganizationOrderBy::CreatedAt);
        let order_direction = order_direction.unwrap_or(OrganizationOrderDirection::Asc);

        let order_by_column = match order_by {
            OrganizationOrderBy::CreatedAt => "created_at",
        };

        let order_dir = match order_direction {
            OrganizationOrderDirection::Asc => "ASC",
            OrganizationOrderDirection::Desc => "DESC",
        };

        let client = self
            .pool
            .get()
            .await
            .context("Failed to get database connection")
            .map_err(RepositoryError::PoolError)?;

        let rows = client
            .query(
                &format!(
                    "
                    SELECT DISTINCT o.* FROM organizations o
                    INNER JOIN organization_members om ON o.id = om.organization_id
                    WHERE om.user_id = $1 AND o.is_active = true
                    ORDER BY o.{order_by_column} {order_dir}
                    LIMIT $2 OFFSET $3
                "
                ),
                &[&user_id, &limit, &offset],
            )
            .await
            .map_err(Self::map_db_error)?;

        let mut organizations = Vec::new();
        for row in rows {
            let db_org = self
                .row_to_db_organization(row)
                .map_err(RepositoryError::DataConversionError)?;
            let domain_org = self
                .db_to_domain_organization(db_org)
                .await
                .map_err(RepositoryError::DataConversionError)?;
            organizations.push(domain_org);
        }

        Ok(organizations)
    }
}
